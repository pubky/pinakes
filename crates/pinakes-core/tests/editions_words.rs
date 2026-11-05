//! What names a later edition (`editions`): its disambiguation, or its title past its release
//! group's; "edition" alone does not, nor does a group's own title.
use pinakes_core::also_on::editions::{edition, later_edition};
use serde_json::json;

#[test]
fn edition_alone_names_no_later_edition() {
    for original in [
        "standard edition",
        "original edition",
        "explicit edition",
        "",
        "Apple Digital Master",
        "Mastered for iTunes",
    ] {
        assert!(!edition(original), "{original}");
    }
    for later in [
        "Legacy edition",
        "10th anniversary edition",
        "special edition",
        "super deluxe",
        "88.2kHz/24‐bit",
    ] {
        assert!(edition(later), "{later}");
    }
}

#[test]
fn a_title_past_its_groups_names_a_later_edition() {
    let release = |title: &str, group: &str, disambiguation: &str| json!({"title": title, "disambiguation": disambiguation, "release-group": {"title": group}});
    assert!(later_edition(&release(
        "Random Access Memories (Deluxe Edition)",
        "Random Access Memories",
        ""
    )));
    assert!(!later_edition(&release(
        "Random Access Memories",
        "Random Access Memories",
        ""
    )));
    // An album called so.
    assert!(!later_edition(&release("Remastered", "Remastered", "")));
}
