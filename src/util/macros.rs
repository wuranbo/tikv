// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

//! The macros crate contains all useful needed macros.

/// Get the count of macro's arguments.
///
/// # Examples
///
/// ```
/// # #[macro_use] extern crate tikv;
/// # fn main() {
/// assert_eq!(count_args!(), 0);
/// assert_eq!(count_args!(1), 1);
/// assert_eq!(count_args!(1, 2), 2);
/// assert_eq!(count_args!(1, 2, 3), 3);
/// # }
/// ```
///
/// [rfc#88](https://github.com/rust-lang/rfcs/pull/88) proposes to use $# to count the number
/// of args, but it has not been implemented yet.
#[macro_export]
macro_rules! count_args {
    () => { 0 };
    ($head:expr $(, $tail:expr)*) => { 1 + count_args!($($tail),*) };
}

/// Initial a HashMap with specify key-value pairs.
///
/// # Examples
///
/// ```
/// # #[macro_use] extern crate tikv;
/// # use std::collections::HashMap;
/// # fn main() {
/// // empty map
/// let m: HashMap<u8, u8> = map!();
/// assert!(m.is_empty());
///
/// // one initial kv pairs.
/// let m = map!("key" => "value");
/// assert_eq!(m.len(), 1);
/// assert_eq!(m["key"], "value");
///
/// // initialize with multiple kv pairs.
/// let m = map!("key1" => "value1", "key2" => "value2");
/// assert_eq!(m.len(), 2);
/// assert_eq!(m["key1"], "value1");
/// assert_eq!(m["key2"], "value2");
/// # }
/// ```
///
/// This macro may be removed once
/// [official implementation](https://github.com/rust-lang/rfcs/issues/542) is provided.
#[macro_export]
macro_rules! map {
    () => {
        {
            use std::collections::HashMap;
            HashMap::new()
        }
    };
    ( $( $k:expr => $v:expr ),+ ) => {
        {
            use std::collections::HashMap;
            let mut temp_map = HashMap::with_capacity(count_args!($(($k, $v)),+));
            $(
                temp_map.insert($k, $v);
            )+
            temp_map
        }
    };
}

/// box try will box error first, and then do the same thing as try!.
#[macro_export]
macro_rules! box_try {
    ($expr:expr) => ({
        match $expr {
            Ok(r) => r,
            Err(e) => return Err(box_err!(e)),
        }
    })
}

/// A shortcut to box an error.
#[macro_export]
macro_rules! box_err {
    ($e:expr) => ({
        use std::error::Error;
        let e: Box<Error + Sync + Send> = ($e).into();
        e.into()
    });
    ($f:tt, $($arg:expr),+) => ({
        box_err!(format!($f, $($arg),+))
    });
}

/// Recover from panicable closure.
///
/// Please note that this macro assume the closure is able to be forced to implement `RecoverSafe`.
/// Also see https://doc.rust-lang.org/nightly/std/panic/trait.RecoverSafe.html
// Maybe we should define a recover macro too.
#[macro_export]
macro_rules! recover_safe {
    ($e:expr) => ({
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use $crate::util::panic_hook;
        panic_hook::mute();
        let res = catch_unwind(AssertUnwindSafe($e));
        panic_hook::unmute();
        res
    })
}

/// Log slow operations with warn!.
macro_rules! slow_log {
    ($t:expr, $($arg:tt)*) => {{
        if $t.is_slow() {
            warn!("{} [takes {:?}]", format_args!($($arg)*), $t.elapsed());
        }
    }}
}

/// make a thread name with additional tag inheriting from current thread.
#[macro_export]
macro_rules! thd_name {
    ($name:expr) => ({
        $crate::util::get_tag_from_thread_name().map(|tag| {
            format!("{}::{}", $name, tag)
        }).unwrap_or_else(|| $name.to_owned())
    });
}

/// Simulating go's defer.
///
/// Please note that, different from go, this defer is bound to scope.
#[macro_export]
macro_rules! defer {
    ($t:expr) => (
        let __ctx = $crate::util::DeferContext::new(|| $t);
    );
}
