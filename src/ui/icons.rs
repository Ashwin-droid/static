//! The 14 bundled SVG icons, loaded once into `iced::widget::svg::Handle`s.
//!
//! Same inventory as the GTK shell (shell spec §3.10). Handles are cheap to
//! clone (`Arc` internally); call sites should clone freely.

use std::sync::LazyLock;

use iced::widget::svg;

macro_rules! icon {
    ($name:ident, $file:literal) => {
        pub fn $name() -> svg::Handle {
            static HANDLE: LazyLock<svg::Handle> = LazyLock::new(|| {
                svg::Handle::from_memory(
                    include_bytes!(concat!("../../assets/icons/", $file)).as_slice(),
                )
            });
            HANDLE.clone()
        }
    };
}

icon!(prev, "prev.svg");
icon!(play, "play.svg");
icon!(pause, "pause.svg");
icon!(next, "next.svg");
icon!(like, "like.svg");
icon!(dislike, "dislike.svg");
icon!(menu, "menu.svg");
icon!(volume, "volume.svg");
icon!(repeat, "repeat.svg");
icon!(repeat_one, "repeat_one.svg");
icon!(shuffle, "shuffle.svg");
icon!(download, "download.svg");
icon!(sleep, "sleep.svg");
icon!(share, "share.svg");
