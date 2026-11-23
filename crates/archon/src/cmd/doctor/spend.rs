use std::path::Path;

use archon_queue::Store;

use super::{Check, Level, check};

const NAME: &str = "spend links";
const MAX_AGENTS: usize = 20;

pub(super) fn check_spend(path: &Path) -> Check {
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some(archon_core::paths::STORE_FILE_NAME)
    );
    match path.try_exists() {
        Ok(false) => return check(NAME, Level::Ok, "no Archon store yet"),
        Ok(true) => {}
        Err(error) => return check(NAME, Level::Warn, format!("cannot locate store: {error}")),
    }
    let agents = match Store::open_live_reader(path).and_then(|store| store.unlinked_spend()) {
        Ok(agents) => agents,
        Err(error) => return check(NAME, Level::Warn, format!("could not check spend: {error}")),
    };
    if agents.is_empty() {
        return check(NAME, Level::Ok, "no unlinked spend rows");
    }
    let ids = agents
        .iter()
        .take(MAX_AGENTS)
        .map(|agent| agent.as_str())
        .collect::<Vec<_>>();
    assert!(!ids.is_empty() && ids.len() <= MAX_AGENTS);
    let omitted = agents.len() - ids.len();
    let more = if omitted == 0 {
        String::new()
    } else {
        format!(" (+{omitted} more)")
    };
    check(
        NAME,
        Level::Warn,
        format!(
            "{} unlinked spend rows: {}{more}",
            agents.len(),
            ids.join(", ")
        ),
    )
}
