use nanoserde::DeJson;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum TagState {
    Empty,
    Occupied,
    Selected,
}

#[derive(Debug, DeJson)]
pub struct MmsgOutput {
    all_tags: Vec<MonitorTags>,
}

#[derive(Debug, DeJson)]
struct MonitorTags {
    monitor: String,
    tags: [Tag; 9],
}

#[derive(Debug, DeJson)]
struct Tag {
    is_active: bool,
    is_urgent: bool,
    client_count: u32,
}

pub fn mmsg_to_tags(msg_output: MmsgOutput) -> Vec<(String, [TagState; 9])> {
    msg_output
        .all_tags
        .into_iter()
        .map(|MonitorTags { monitor, tags }| {
            let tags = tags.map(|t| {
                if t.is_active {
                    TagState::Selected
                } else if t.client_count > 0 {
                    TagState::Occupied
                } else {
                    TagState::Empty
                }
            });
            (monitor, tags)
        })
        .collect()
}

pub fn poll_tags() -> anyhow::Result<MmsgOutput> {
    let output = std::process::Command::new("mmsg")
        .args(["get", "all-tags"])
        .output()?;

    let output = String::from_utf8(output.stdout)?;

    Ok(MmsgOutput::deserialize_json(&output)?)
}
