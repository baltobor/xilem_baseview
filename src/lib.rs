//! This file is part of the xilem_baseview project.
//! (c) 2026 by Jacek Wisniowski
//!
//! This project was released as open source under the
//! Apache License, Version 2.0: http://www.apache.org/licenses/LICENSE-2.0
//! (compatible with Xilem).
//!
//! Xilem Baseview - Xilem reactive UI for audio plugins
//!
//! This crate enables Xilem's declarative reactive UI in audio plugins via baseview.
//! It is the baseview equivalent of the upstream `xilem` crate (which uses winit).
//!
//! # Architecture
//!
//!
//! - xilem_core, reactive View trait
//! - xilem_masonry, View to Widget bridge, all views
//! - masonry, widget toolkit
//!
//! - xilem_baseview
//!     - app.rs, XilemBaseview builder
//!     - driver.rs, Xilem reactive cycle
//!     - handler.rs, baseview::WindowHandler
//!     - event.rs, Event translation
//!     - render.rs, GPU rendering
//!
//!
//! # Usage
//!
//! ```ignore
//! use xilem_baseview::prelude::*;
//! use xilem_baseview::XilemBaseview;
//!
//! struct Counter(i32);
//!
//! fn app_logic(data: &mut Counter) -> impl WidgetView<Edit<Counter>> {
//!     flex_col((
//!         label(format!("{}", data.0)),
//!         text_button("increment", |data: &mut Counter| data.0 += 1),
//!     ))
//! }
//!
//! XilemBaseview::new(Counter(0), app_logic)
//!     .open_blocking(WindowOpenOptions {
//!         title: "Counter".into(),
//!         size: Size::new(300.0, 200.0),
//!         scale: WindowScalePolicy::SystemScaleFactor,
//!     });
//! ```

mod app;
mod driver;
pub(crate) mod event;
mod handler;
pub(crate) mod render;

// Public API
pub use app::{XilemBaseview, XilemBaseviewHandle};

// Re-export baseview types needed for window creation
pub use baseview::{Size, WindowOpenOptions, WindowScalePolicy};

// Re-export masonry types commonly used in app code
pub use masonry;
pub use masonry::dpi;
pub use masonry::palette;
pub use masonry::peniko::{Blob, Color};

// Re-export xilem_masonry view types and traits
pub use xilem_masonry;
pub use xilem_masonry::core;
pub use xilem_masonry::style;
pub use xilem_masonry::view;
pub use xilem_masonry::{AnyWidgetView, MasonryRoot, Pod, ViewCtx, WidgetView};

/// Convenience re-exports for common usage.
pub mod prelude {
    pub use crate::view::*;
    pub use crate::{
        AnyWidgetView, Size, ViewCtx, WidgetView, WindowOpenOptions, WindowScalePolicy,
        XilemBaseview,
    };
    pub use masonry::peniko::Color;
    pub use xilem_masonry::core::Edit;
}
