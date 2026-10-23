/*
 * Copyright (C) 2017 Genymobile
 * Copyright (C) 2026 David MANZI
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::{fmt, sync::atomic::Ordering};

use atomic_enum::atomic_enum;
use chrono::Local;

static THRESHOLD: AtomicLevelFilter = AtomicLevelFilter::new(LevelFilter::Info);

pub fn set_threshold(level: LevelFilter) {
    THRESHOLD.store(level, Ordering::Relaxed);
}

pub fn is_enabled(level: LevelFilter) -> bool {
    level <= THRESHOLD.load(Ordering::Relaxed)
}

#[atomic_enum]
#[derive(PartialEq, PartialOrd)]
pub enum LevelFilter {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl fmt::Display for LevelFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LevelFilter::Off => write!(f, ""),
            LevelFilter::Error => write!(f, "ERROR"),
            LevelFilter::Warn => write!(f, "WARN"),
            LevelFilter::Info => write!(f, "INFO"),
            LevelFilter::Debug => write!(f, "DEBUG"),
            LevelFilter::Trace => write!(f, "TRACE"),
        }
    }
}

#[doc(hidden)]
pub fn emit(level: LevelFilter, target: &str, args: std::fmt::Arguments<'_>) {
    if is_enabled(level) {
        let date = Local::now();
        let formatted_date = date.format("%Y-%m-%d %H:%M:%S%.3f");
        let msg = format!("{} {} {}: {}", formatted_date, level, target, args);
        if level == LevelFilter::Error {
            eprintln!("{}", msg);
        } else {
            println!("{}", msg);
        }
    }
}

#[macro_export]
macro_rules! info {
    (target: $target:expr, $($arg:tt)+) => {
        $crate::logger::emit($crate::logger::LevelFilter::Info, $target, format_args!($($arg)+))
    };
}

#[macro_export]
macro_rules! warn {
    (target: $target:expr, $($arg:tt)+) => {
        $crate::logger::emit($crate::logger::LevelFilter::Warn, $target, format_args!($($arg)+))
    };
}

#[macro_export]
macro_rules! error {
    (target: $target:expr, $($arg:tt)+) => {
        $crate::logger::emit($crate::logger::LevelFilter::Error, $target, format_args!($($arg)+))
    };
}

#[macro_export]
macro_rules! debug {
    (target: $target:expr, $($arg:tt)+) => {
            $crate::logger::emit($crate::logger::LevelFilter::Debug, $target, format_args!($($arg)+))
    };
}

#[macro_export]
macro_rules! trace {
    (target: $target:expr, $($arg:tt)+) => {
            $crate::logger::emit($crate::logger::LevelFilter::Trace, $target, format_args!($($arg)+))
    };
}
