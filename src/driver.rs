//! This file is part of the xilem_baseview project.
//! (c) 2026 by Jacek Wisniowski
//!
//! This project was released as open source under the
//! Apache License, Version 2.0: http://www.apache.org/licenses/LICENSE-2.0
//! (compatible with Xilem).
//!
//! Xilem reactive cycle for baseview
//!
//! This is the core of the Xilem integration: it owns the application state,
//! runs the app logic to produce views, and diffs views to update the widget tree.
//!
//! Modeled on upstream xilem/src/driver.rs but simplified for single-window use.
//!
//! NOTE: Multi-window support
//! To support multiple windows, this struct would need a HashMap<WindowId, WindowState>
//! similar to upstream xilem's driver.rs, and the app logic would return an iterator
//! of WindowView<State> instead of a single WidgetView. See xilem/src/driver.rs and
//! xilem/src/window_view.rs for the multi-window pattern.

use std::fmt::Debug;
use std::sync::Arc;

use masonry::app::RenderRoot;
use masonry::core::{ErasedAction, WidgetId};
use masonry::peniko::Blob;
use masonry::widgets::Passthrough;
use xilem_masonry::core::{
    DynMessage, Edit, MessageCtx, MessageResult, ProxyError, RawProxy, SendMessage, View, ViewId,
    ViewPathTracker,
};
use xilem_masonry::{InitialRootWidget, MasonryRoot, ViewCtx, WidgetView};

/// The Xilem reactive driver for baseview.
///
/// Owns the root state, runs the app logic, and manages view diffing.
pub(crate) struct BaseviewDriver<State: 'static, Logic> {
    pub(crate) state: State,
    pub(crate) logic: Logic,
    root_view: Option<MasonryRoot<State>>,
    view_ctx: Option<ViewCtx>,
    view_state: Option<
        <MasonryRoot<State> as View<Edit<State>, (), ViewCtx>>::ViewState,
    >,
    proxy: Arc<BaseviewProxy>,
    runtime: Arc<tokio::runtime::Runtime>,
    fonts: Vec<Blob<u8>>,
}

/// The type used to send async messages back to the driver.
pub(crate) type MessagePackage = (Arc<[ViewId]>, SendMessage);

/// Proxy for sending async messages from task/worker views.
pub(crate) struct BaseviewProxy {
    sender: std::sync::Mutex<tokio::sync::mpsc::UnboundedSender<MessagePackage>>,
}

impl Debug for BaseviewProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseviewProxy").finish_non_exhaustive()
    }
}

impl RawProxy for BaseviewProxy {
    fn send_message(&self, path: Arc<[ViewId]>, message: SendMessage) -> Result<(), ProxyError> {
        self.sender
            .lock()
            .unwrap()
            .send((path, message))
            .map_err(|e| ProxyError::DriverFinished(e.0 .1))
    }

    fn dyn_debug(&self) -> &dyn Debug {
        self
    }
}

impl<State, Logic, View> BaseviewDriver<State, Logic>
where
    State: 'static,
    Logic: FnMut(&mut State) -> View,
    View: WidgetView<Edit<State>>,
{
    /// Create a new driver and build the initial widget tree.
    ///
    /// Returns the driver and the async message receiver (to be polled each frame).
    pub(crate) fn new(
        state: State,
        logic: Logic,
        runtime: Arc<tokio::runtime::Runtime>,
        fonts: Vec<Blob<u8>>,
    ) -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<MessagePackage>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let proxy = Arc::new(BaseviewProxy {
            sender: std::sync::Mutex::new(tx),
        });

        let driver = Self {
            state,
            logic,
            root_view: None,
            view_ctx: None,
            view_state: None,
            proxy,
            runtime,
            fonts,
        };

        (driver, rx)
    }

    /// Build the initial view and widget tree.
    /// Returns the initial root widget to be inserted into the RenderRoot.
    pub(crate) fn build_initial(&mut self) -> InitialRootWidget {
        let view = (self.logic)(&mut self.state);
        let masonry_root = MasonryRoot::new(view);

        let mut view_ctx = ViewCtx::new(self.proxy.clone(), self.runtime.clone());
        let (initial_widget, view_state) = masonry_root.build(&mut view_ctx, &mut self.state);

        self.root_view = Some(masonry_root);
        self.view_ctx = Some(view_ctx);
        self.view_state = Some(view_state);

        initial_widget
    }

    /// Handle a widget action (from signal sink).
    /// Routes through the view message system and triggers rebuild if needed.
    pub(crate) fn handle_action(
        &mut self,
        render_root: &mut RenderRoot,
        widget_id: WidgetId,
        action: ErasedAction,
    ) {
        let view_ctx = self.view_ctx.as_mut().unwrap();
        let Some(id_path) = view_ctx.get_id_path(widget_id) else {
            tracing::error!(
                "Got action {action:?} for unknown widget. Did you forget to use `with_action_widget`?"
            );
            return;
        };
        let id_path = id_path.clone();
        let message = DynMessage(action);

        let result = self.dispatch_message(render_root, id_path, message);
        self.handle_message_result(render_root, result);
    }

    /// Handle an async message (from task/worker views via the proxy).
    pub(crate) fn handle_async_action(
        &mut self,
        render_root: &mut RenderRoot,
        path: Arc<[ViewId]>,
        message: SendMessage,
    ) {
        let id_path = Vec::from(&*path);
        let message: DynMessage = message.into();

        let result = self.dispatch_message(render_root, id_path, message);
        self.handle_message_result(render_root, result);
    }

    fn dispatch_message(
        &mut self,
        render_root: &mut RenderRoot,
        id_path: Vec<ViewId>,
        message: DynMessage,
    ) -> MessageResult<()> {
        let root_view = self.root_view.as_ref().unwrap();
        let view_ctx = self.view_ctx.as_mut().unwrap();
        let view_state = self.view_state.as_mut().unwrap();

        let mut message_context = MessageCtx::new(
            std::mem::take(view_ctx.environment()),
            id_path,
            message,
        );

        let result = root_view.message(view_state, &mut message_context, render_root, &mut self.state);

        let (env, _id_path, _message) = message_context.finish();
        *view_ctx.environment() = env;

        result
    }

    fn handle_message_result(
        &mut self,
        render_root: &mut RenderRoot,
        result: MessageResult<()>,
    ) {
        match result {
            MessageResult::Action(()) => {
                self.run_logic(render_root);
            }
            MessageResult::RequestRebuild => {
                let root_view = self.root_view.as_ref().unwrap();
                let view_ctx = self.view_ctx.as_mut().unwrap();
                let view_state = self.view_state.as_mut().unwrap();
                root_view.rebuild(root_view, view_state, view_ctx, render_root, &mut self.state);
            }
            MessageResult::Nop => {}
            MessageResult::Stale => {
                tracing::info!("Discarding stale message");
            }
        }
    }

    /// Re-run the app logic and diff the new view tree against the old one.
    pub(crate) fn run_logic(&mut self, render_root: &mut RenderRoot) {
        let new_view = (self.logic)(&mut self.state);
        let new_root = MasonryRoot::new(new_view);

        let prev_root = self.root_view.as_ref().unwrap();
        let view_ctx = self.view_ctx.as_mut().unwrap();
        let view_state = self.view_state.as_mut().unwrap();

        new_root.rebuild(prev_root, view_state, view_ctx, render_root, &mut self.state);

        self.root_view = Some(new_root);
    }

    /// Register custom fonts with the render root.
    pub(crate) fn register_fonts(&mut self, render_root: &mut RenderRoot) {
        let fonts = std::mem::take(&mut self.fonts);
        for font in &fonts {
            let blob: Blob<u8> = font.clone();
            drop(render_root.register_fonts(blob));
        }
    }

    /// Set up focus fallback on the root passthrough widget.
    pub(crate) fn set_focus_fallback(&self, render_root: &mut RenderRoot) {
        let layer_root = render_root.get_layer_root(0);
        if let Some(root_widget) = layer_root.downcast::<Passthrough>() {
            let fallback = root_widget.inner().inner_id();
            render_root.set_focus_fallback(Some(fallback));
        }
    }
}
