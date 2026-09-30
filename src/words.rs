pub const ADJECTIVES: &[&str] = &[
    "azure", "crimson", "golden", "silver", "quiet", "swift", "bold", "gentle", "bright", "dark",
    "ancient", "modern", "rustic", "cosmic", "amber", "frozen", "hidden", "lucky", "quantum",
    "velvet",
];

pub const NOUNS: &[&str] = &[
    "falcon", "harbor", "lantern", "meadow", "river", "summit", "compass", "ember", "grove",
    "signal", "anchor", "orbit", "canyon", "beacon", "thicket", "prairie", "glacier", "current",
    "horizon", "cinder",
];

pub fn random_name(rng: &mut impl rand::Rng) -> String {
    use rand::seq::IndexedRandom;
    let adj = ADJECTIVES.choose(rng).unwrap();
    let noun = NOUNS.choose(rng).unwrap();
    format!("{adj}-{noun}")
}
