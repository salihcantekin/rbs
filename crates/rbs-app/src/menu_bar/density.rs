//! The View › Spacing densities by their command and API value (ADR-0113 decision 2).

/// The command and API value of a density (`regular`, `compact`, `roomy`).
pub fn density_id(density: rbs_ui::tokens::Density) -> &'static str {
    match density {
        rbs_ui::tokens::Density::Regular => "regular",
        rbs_ui::tokens::Density::Compact => "compact",
        rbs_ui::tokens::Density::Roomy => "roomy",
    }
}

/// The density named `id`, if any.
pub fn density_from_id(id: &str) -> Option<rbs_ui::tokens::Density> {
    use rbs_ui::tokens::Density;
    [Density::Regular, Density::Compact, Density::Roomy]
        .into_iter()
        .find(|d| density_id(*d) == id)
}
