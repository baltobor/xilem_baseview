//! This file is part of the xilem_baseview project.
//! (c) 2026 by Jacek Wisniowski
//!
//! This project was released as open source under the
//! Apache License, Version 2.0: http://www.apache.org/licenses/LICENSE-2.0
//! (compatible with Xilem).
//!
//! Example demonstrating xilem_synth_widgets with two Knobs and a Fader in a Group box.
//!
//! Run with: cargo run --example synth_widgets

use xilem_baseview::prelude::*;
use xilem_baseview::XilemBaseview;
use xilem_synth_widgets::{fader, group_box, knob};

struct SynthState {
    knob1_value: f64,
    knob2_value: f64,
    fader_value: f32,
}

fn app_logic(state: &mut SynthState) -> impl WidgetView<Edit<SynthState>> {
    // Note: Explicit type parameters on flex_col/flex_row help Rust's type inference
    // with xilem_synth_widgets views in flex containers.
    flex_col::<Edit<SynthState>, (), _>((
        group_box(
            "Controls",
            flex_row::<Edit<SynthState>, (), _>((
                flex_col::<Edit<SynthState>, (), _>((
                    knob::<Edit<SynthState>, ()>(0.0, 100.0, state.knob1_value, 50.0, |s, v| {
                        s.knob1_value = v
                    }),
                    label("Frequency"),
                )),
                flex_col::<Edit<SynthState>, (), _>((
                    knob::<Edit<SynthState>, ()>(0.0, 100.0, state.knob2_value, 50.0, |s, v| {
                        s.knob2_value = v
                    }),
                    label("Resonance"),
                )),
                flex_col::<Edit<SynthState>, (), _>((
                    fader::<Edit<SynthState>, ()>(-60.0, 6.0, state.fader_value as f64, -12.0, |s, v| {
                        s.fader_value = v as f32
                    }),
                    label("Volume"),
                )),
            )),
        ),
        label(format!(
            "Freq: {:.1} | Res: {:.1} | Vol: {:.1} dB",
            state.knob1_value, state.knob2_value, state.fader_value
        )),
    ))
}

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::INFO)
        .init();

    println!("Opening Xilem synth widgets demo...");

    let initial_state = SynthState {
        knob1_value: 50.0,
        knob2_value: 50.0,
        fader_value: -12.0,
    };

    XilemBaseview::new(initial_state, app_logic).open_blocking(WindowOpenOptions {
        title: "Synth Widgets in xilem_baseview".into(),
        size: Size::new(400.0, 350.0),
        scale: WindowScalePolicy::SystemScaleFactor,
    });

    println!("Window closed.");
}
