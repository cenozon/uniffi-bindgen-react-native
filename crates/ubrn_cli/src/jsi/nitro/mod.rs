/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/
 */
pub(crate) mod codegen;

pub(crate) use self::codegen::{get_files, get_files_for_android, get_files_for_ios};
