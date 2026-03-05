//! This file is part of the xilem_baseview project.
//! (c) 2026 by Jacek Wisniowski
//!
//! This project was released as open source under the
//! Apache License, Version 2.0: http://www.apache.org/licenses/LICENSE-2.0
//! (compatible with Xilem).
//!
//! Public API for creating Xilem windows in baseview.
//!
//! Provides the `XilemBaseview` builder which is the entry point for creating
//! Xilem-powered audio plugin UIs.
//!
//! NOTE: Multi-window support
//! To support multiple windows, the builder would need to accept a multi-window
//! app logic function (returning an iterator of WindowView<State>) and manage
//! multiple baseview windows. See xilem/src/app.rs for the multi-window pattern.

use std::sync::Arc;

use baseview::{Window, WindowOpenOptions};
use masonry::peniko::Blob;
use raw_window_handle::HasRawWindowHandle;
use xilem_masonry::WidgetView;

use crate::driver::BaseviewDriver;
use crate::handler::XilemHandler;

/// Handle to a Xilem window running in baseview.
pub struct XilemBaseviewHandle {
    // Currently empty - baseview handles are fire-and-forget.
    // In future could add a communication channel for shutdown, etc.
}

/// Builder for creating Xilem-powered baseview windows.
///
/// # Example
///
/// ```ignore
/// use xilem_baseview::prelude::*;
/// use xilem_baseview::XilemBaseview;
///
/// struct AppState { count: i32 }
///
/// fn app_logic(state: &mut AppState) -> impl WidgetView<AppState> {
///     // build your view tree here
/// }
///
/// XilemBaseview::new(AppState { count: 0 }, app_logic)
///     .open_blocking(WindowOpenOptions { ... });
/// ```
#[must_use = "A XilemBaseview app does nothing unless opened."]
pub struct XilemBaseview<State, Logic> {
    state: State,
    logic: Logic,
    runtime: Arc<tokio::runtime::Runtime>,
    fonts: Vec<Blob<u8>>,
}

impl<State, Logic, View> XilemBaseview<State, Logic>
where
    State: Send + 'static,
    Logic: FnMut(&mut State) -> View + Send + 'static,
    View: WidgetView<State>,
{
    /// Create a new Xilem app builder.
    pub fn new(state: State, logic: Logic) -> Self {
        Self {
            state,
            logic,
            runtime: Arc::new(tokio::runtime::Runtime::new().unwrap()),
            fonts: Vec::new(),
        }
    }

    /// Create a new Xilem app builder with an existing tokio runtime.
    pub fn new_with_runtime(
        state: State,
        logic: Logic,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Self {
        Self {
            state,
            logic,
            runtime,
            fonts: Vec::new(),
        }
    }

    /// Load a font when this app is opened.
    pub fn with_font(mut self, data: impl Into<Blob<u8>>) -> Self {
        self.fonts.push(data.into());
        self
    }

    /// Open a window parented to another window (for plugin UIs).
    ///
    /// This is the primary method for CLAP/VST plugin integration.
    /// The parent handle comes from the audio plugin host.
    pub fn open_parented<P>(self, parent: &P, options: WindowOpenOptions) -> XilemBaseviewHandle
    where
        P: HasRawWindowHandle,
    {
        let width = options.size.width;
        let height = options.size.height;

        // Pass Send-safe components through to the window thread.
        // The driver and handler are created on the window thread itself
        // because ViewCtx contains non-Send types (Environment has dyn Any).
        let state = self.state;
        let logic = self.logic;
        let runtime = self.runtime;
        let fonts = self.fonts;

        let cell = std::sync::Mutex::new(Some((state, logic, runtime, fonts)));

        Window::open_parented(parent, options, move |_| {
            let (state, logic, runtime, fonts) = cell.lock().unwrap().take().unwrap();
            let (driver, async_rx) = BaseviewDriver::new(state, logic, runtime, fonts);
            XilemHandler::new(driver, async_rx, width, height)
        });

        XilemBaseviewHandle {}
    }

    /// Open a standalone window (for testing outside a plugin host).
    ///
    /// This blocks the current thread until the window is closed.
    pub fn open_blocking(self, options: WindowOpenOptions) {
        let width = options.size.width;
        let height = options.size.height;

        let state = self.state;
        let logic = self.logic;
        let runtime = self.runtime;
        let fonts = self.fonts;

        let cell = std::sync::Mutex::new(Some((state, logic, runtime, fonts)));

        Window::open_blocking(options, move |_| {
            let (state, logic, runtime, fonts) = cell.lock().unwrap().take().unwrap();
            let (driver, async_rx) = BaseviewDriver::new(state, logic, runtime, fonts);
            XilemHandler::new(driver, async_rx, width, height)
        });
    }
}
