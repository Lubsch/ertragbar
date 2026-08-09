use crate::{battery::Battery, network::Network, tags::TagState, time::{Date, Time}};

mod tags;
mod network;
mod battery;
mod time;
mod render;
mod gamma;

#[derive(Debug)]
struct Bar {
    tags: [TagState; 9],
    network: Network,
    battery: Battery,
    date: Date,
    time: Time
}

mod wayland;

fn main() -> anyhow::Result<()> {
    let font = std::fs::read("/nix/store/l9lig0cpdxwyjmhx6yb2pr1lsx29rs78-adwaita-fonts-50.0/share/fonts/Adwaita/AdwaitaSans-Regular.ttf")?;
    let font = fontdue::Font::from_bytes(font.as_slice(), fontdue::FontSettings::default())
        .map_err(|e| anyhow::anyhow!("font parse: {e}"))?;
    wayland::run(font)
}
