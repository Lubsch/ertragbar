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
    let font = std::fs::read("/nix/store/b47vj9qv7rx6g72hcpdkpvf43gjrnvfj-adwaita-fonts-50.0/share/fonts/Adwaita/AdwaitaSans-Regular.ttf")?;
    let start = std::time::Instant::now();
    let font = fontdue::Font::from_bytes(font.as_slice(), fontdue::FontSettings::default())
        .map_err(|e| anyhow::anyhow!("font parse: {e}"))?;
    let end = std::time::Instant::now();
    dbg!(end-start);
    wayland::run(font)
}
