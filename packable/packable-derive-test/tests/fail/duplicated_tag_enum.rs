// Copyright 2023 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

#![allow(unused_imports, unreachable_patterns)]

use core::convert::Infallible;

use packable::{error::UnknownTagError, Packable};

#[derive(Packable)]
#[packable(tag_type = u8)]
#[packable(unpack_error = UnknownTagError<u8>)]
pub enum OptI32 {
    #[packable(tag = 0)]
    None,
    #[packable(tag = 0)]
    Some(i32),
}

fn main() {}
