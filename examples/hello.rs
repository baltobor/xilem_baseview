//! This file is part of the xilem_baseview project.
//! (c) 2026 by Jacek Wisniowski
//!
//! This project was released as open source under the
//! Apache License, Version 2.0: http://www.apache.org/licenses/LICENSE-2.0
//! (compatible with Xilem).
//!
//! Simple counter example: label + increment/decrement buttons
//!
//! Run with: cargo run --example hello

use xilem_baseview::prelude::*;
use xilem_baseview::XilemBaseview;

struct Counter {
    count: i32,
}

fn app_logic(data: &mut Counter) -> impl WidgetView<Edit<Counter>> {
    flex_col((
        label(format!("Count: {}", data.count)),
        text_button("Increment", |data: &mut Counter| data.count += 1),
        text_button("Decrement", |data: &mut Counter| data.count -= 1),
    ))
}

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::INFO)
        .init();

    println!("Opening Xilem baseview window...");

    XilemBaseview::new(Counter { count: 0 }, app_logic).open_blocking(WindowOpenOptions {
        title: "Xilem Baseview Counter".into(),
        size: Size::new(300.0, 200.0),
        scale: WindowScalePolicy::SystemScaleFactor,
    });

    println!("Window closed.");
}
