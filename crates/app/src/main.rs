use gpui_kit::component::{TitleBar, h_flex, status_bar::StatusBar, v_flex};
use gpui_kit::*;

struct WorkspaceShell;

impl Render for WorkspaceShell {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(
                TitleBar::new().child(
                    h_flex()
                        .w_full()
                        .pr_2()
                        .justify_between()
                        .child("editing")
                        .child("No photos open"),
                ),
            )
            .child(
                div()
                    .id("workspace")
                    .flex_1()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .child("Workspace"),
            )
            .child(StatusBar::new().left("Ready").right("editing"))
    }
}

fn main() {
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);

        let window_options = TitleBar::window_options();

        gpui_kit::open_window(window_options, cx, |_, cx| cx.new(|_| WorkspaceShell))
            .expect("Failed to open window");
    });
}
