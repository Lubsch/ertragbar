use nanoserde::DeJson;

#[derive(Debug, PartialEq)]
pub enum TagState {
    Empty,
    Occupied,
    Selected,
}

#[derive(Debug, DeJson)]
pub struct MmsgOutput {
    all_tags: Vec<MonitorTags>
}

#[derive(Debug, DeJson)]
struct MonitorTags {
    monitor: String,
    tags: [Tag; 9]
}

#[derive(Debug, DeJson)]
struct Tag {
    is_active: bool,
    is_urgent: bool,
    client_count: u32
}

pub fn mmsg_to_tags(mut msg_output: MmsgOutput) -> anyhow::Result<[TagState; 9]> {
    let monitor = msg_output.all_tags.pop().unwrap();

    let tags = monitor.tags.map(|t| {
            if t.is_active {
                TagState::Selected
            } else if t.client_count > 0 {
                TagState::Occupied
            } else {
                TagState::Empty
            }
        });

    Ok(tags)
}

pub fn poll_tags() -> anyhow::Result<MmsgOutput> {
    let output = std::process::Command::new("mmsg")
        .args(["get", "all-tags"])
        .output()?;

    let output = String::from_utf8(output.stdout)?;

    Ok(MmsgOutput::deserialize_json(&output)?)
}
