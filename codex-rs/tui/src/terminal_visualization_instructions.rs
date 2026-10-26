use crate::legacy_core::config::Config;
use codex_features::Feature;

pub(crate) const TERMINAL_VISUALIZATION_INSTRUCTIONS: &str = "\
- This surface is a terminal. When the formatting rules require a visual, include one in the final answer using compact ASCII diagrams, trees, timelines, or tables.
- Use tables for exact mappings or comparisons rather than collapsing known mappings into prose.
- Use trees for hierarchy or one-to-many relationships, and diagrams or timelines for sequence, change, or state transferred between records across event order.
- Use only ASCII characters in visuals.";

pub(crate) fn with_terminal_visualization_instructions(
    config: &Config,
    control_instructions: Option<String>,
) -> Option<String> {
    let terminal_visualization_enabled = config
        .features
        .enabled(Feature::TerminalVisualizationInstructions);
    let base_instructions = if terminal_visualization_enabled {
        control_instructions.or_else(|| config.developer_instructions.clone())
    } else {
        control_instructions
    };
    let mut sections = base_instructions
        .into_iter()
        .filter(|section| !section.trim().is_empty())
        .collect::<Vec<_>>();
    if terminal_visualization_enabled {
        sections.push(TERMINAL_VISUALIZATION_INSTRUCTIONS.to_string());
    }
    if let Ok(store) =
        crate::ticket_store::TicketStore::from_workspace_roots(&config.workspace_roots)
        && let Ok(index) = store.compact_index()
    {
        sections.push(index);
    }
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}
