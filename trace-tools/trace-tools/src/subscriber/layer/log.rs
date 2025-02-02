// Copyright 2022 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use std::{
    fs::{File, OpenOptions},
    io::{self, Stdout, StdoutLock},
};

use colored::{ColoredString, Colorize};
use fern_logger::{LoggerConfig, LoggerOutputConfig};
use parking_lot::{Mutex, MutexGuard};
use tracing::{metadata::LevelFilter, Event, Level, Metadata, Subscriber};
use tracing_log::{AsTrace, NormalizeEvent};
use tracing_subscriber::{
    filter::{self, Targets},
    fmt::MakeWriter,
    layer::{Context, Filter, Layer},
    registry::LookupSpan,
};

use crate::{subscriber::visitors::MessageVisitor, Error};

/// Describes the output target of a [`log`] event.
///
/// Variants wrap a locked writer to the output target.
enum LogOutput<'a> {
    /// Log to standard output, with optional color.
    Stdout(StdoutLock<'a>, bool),
    /// Log to a file.
    File(MutexGuard<'a, File>),
}

impl<'a> io::Write for LogOutput<'a> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Stdout(lock, _) => lock.write(buf),
            Self::File(lock) => lock.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Stdout(lock, _) => lock.flush(),
            Self::File(lock) => lock.flush(),
        }
    }
}

/// Describes the target destination of a [`log`] event.
///
/// Locks obtained from these targets are used to create writers to the appropriate [`LogOutput`].
enum LogDest {
    /// Log to standard output, with optional color.
    Stdout(bool),
    /// Log to a file.
    File(Mutex<File>),
}

/// Describes a target destination of a [`log`] event, combined with filters that only permit
/// specific events to be logged to that target.
struct LogTarget {
    /// Target filters. Enables/disables [`Span`](tracing::Span)s based on their target and level.
    filter: Targets,
    /// The output destination of the event, if it passes through the filter.
    dest: LogDest,
}

/// [`MakeWriter`] implementation for the [`LogLayer`].
///
/// Constructs a writer for a specific [`LogTarget`].
struct LogTargetMakeWriter {
    stdout: Stdout,
    target: LogTarget,
}

impl LogTargetMakeWriter {
    fn new(target: LogTarget) -> Self {
        Self {
            stdout: io::stdout(),
            target,
        }
    }

    fn enabled<S>(&self, meta: &Metadata<'_>, ctx: &Context<'_, S>) -> bool
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    {
        Filter::enabled(&self.target.filter, meta, ctx)
    }
}

impl<'a> MakeWriter<'a> for &'a LogTargetMakeWriter {
    type Writer = LogOutput<'a>;

    fn make_writer(&self) -> Self::Writer {
        match &self.target.dest {
            LogDest::Stdout(color) => LogOutput::Stdout(self.stdout.lock(), *color),
            LogDest::File(file) => LogOutput::File(file.lock()),
        }
    }
}

/// A [`tracing_subscriber::Layer`] for replicating the logging functionality in
/// [`fern_logger`] without using the [`log`] crate as the global subscriber.
///
/// Without this layer, enabling this crate's [`Subscriber`] will disable all logging of any kind, since
/// it will be used as the global subscriber for the lifetime of the program, and all [`log`] events will
/// be ignored.
///
/// This layer registers an interest in [`Event`]s that describe [`log`] events,
/// generated by [`tracing_log`]. These are only created when
/// [`collect_logs`](crate::subscriber::collect_logs) is called, or a [`LogTracer`](tracing_log::LogTracer)
/// is initialised.
pub struct LogLayer {
    make_writers: Vec<LogTargetMakeWriter>,
    fmt_events: LogFormatter,
}

impl<S> Layer<S> for LogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // If the event is originally issued by the `log` crate, generate the appropriate `tracing` metadata.
        if let Some(metadata) = event.normalized_metadata() {
            let mut buf = String::new();

            for make_writer in &self.make_writers {
                // Only write to an output if the event target is enabled by filters.
                if make_writer.enabled(&metadata, &ctx) {
                    let mut writer = make_writer.make_writer();

                    if self.fmt_events.format_event(&mut buf, &writer, event).is_ok() {
                        let _ = io::Write::write(&mut writer, buf.as_bytes());
                    }

                    buf.clear();
                }
            }
        }
    }
}

impl LogLayer {
    /// The name that specifies the standard output as a log target (instead of a file).
    const STDOUT_NAME: &'static str = "stdout";

    pub(crate) fn new(config: LoggerConfig) -> Result<Self, Error> {
        let fmt_events = LogFormatter {
            target_width: config.target_width(),
            level_width: config.level_width(),
        };

        let make_writers = config
            .outputs()
            .iter()
            .map(|output_config: &LoggerOutputConfig| {
                let level = output_config.level_filter().as_trace();

                let mut targets = if output_config.target_filters().is_empty() {
                    filter::Targets::default().with_default(level)
                } else {
                    let mut targets = filter::Targets::default().with_default(LevelFilter::OFF);

                    for filter in output_config.target_filters() {
                        targets = targets.with_target(filter.clone().to_lowercase(), level);
                    }

                    targets
                };

                for exclusion in output_config.target_exclusions() {
                    targets = targets.with_target(exclusion.clone().to_lowercase(), LevelFilter::OFF);
                }

                let dest = match output_config.name() {
                    Self::STDOUT_NAME => LogDest::Stdout(output_config.color_enabled()),
                    name => {
                        let file = OpenOptions::new().write(true).create(true).append(true).open(name)?;
                        LogDest::File(Mutex::new(file))
                    }
                };

                Ok(LogTargetMakeWriter::new(LogTarget { filter: targets, dest }))
            })
            .collect::<Result<_, io::Error>>()
            .map_err(|err| Error::LogLayer(err.into()))?;

        Ok(Self {
            make_writers,
            fmt_events,
        })
    }
}

/// Trait that allows a type to be formatted into a [`ColoredString`].
///
/// Using a trait here allows this functionality to be implemented for the external [`Level`] type.
trait ColorFormat {
    /// Formats `self` into a [`ColoredString`].
    fn color(self, enabled: bool) -> ColoredString;
}

impl ColorFormat for Level {
    fn color(self, enabled: bool) -> ColoredString {
        let text = self.to_string();

        if !enabled {
            return text.as_str().into();
        }

        match self {
            Level::TRACE => text.bright_magenta(),
            Level::DEBUG => text.bright_blue(),
            Level::INFO => text.bright_green(),
            Level::WARN => text.bright_yellow(),
            Level::ERROR => text.bright_red(),
        }
    }
}

/// Helper struct for formatting [`log`] records into a [`String`] and writing to a [`Write`](std::fmt::Write)
/// implementer.
struct LogFormatter {
    target_width: usize,
    level_width: usize,
}

impl LogFormatter {
    /// Formats a [`log`] record (converted into a [`tracing::Event`] by [`tracing_log`]) into a [`String`].
    ///
    /// This string is then written to a [`Write`](std::fmt::Write) implementer.
    ///
    /// Formatting can change depending on the output target of the writer, and so this must also be
    /// provided. An output that writes to `stdout` can potentially be formatted with text colors.
    fn format_event<W>(&self, writer: &mut W, output: &LogOutput, event: &Event<'_>) -> std::fmt::Result
    where
        W: std::fmt::Write,
    {
        if let Some(metadata) = event.normalized_metadata() {
            let level = *metadata.level();
            let target = metadata.target();

            let mut visitor = MessageVisitor::default();
            event.record(&mut visitor);

            let time = time_helper::format(&time_helper::now_utc());

            let level = match *output {
                LogOutput::File(_) => ColoredString::from(level.to_string().as_str()),
                LogOutput::Stdout(_, color_enabled) => level.color(color_enabled),
            };

            write!(
                writer,
                "{} {:target_width$} {:level_width$} {}",
                time,
                target,
                level,
                visitor.0,
                target_width = self.target_width,
                level_width = self.level_width,
            )?;

            writeln!(writer)?;
        }

        Ok(())
    }
}
