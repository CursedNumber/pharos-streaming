// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Standalone binding generator (uniffi "library mode"). Built from this crate
//! so its uniffi version always matches the scaffolding in `lib.rs`.

fn main() {
    uniffi::uniffi_bindgen_main()
}
