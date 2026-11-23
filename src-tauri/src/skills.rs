//! Is the site's skill list in step with the game's?
//!
//! `sl-admin` writes every skill in the game to `sotalore-game-skills.json` (`"type": "gameSkills"`,
//! `/sladmin gameskills`; sladmin's `EXPORT-FORMAT.md`, "Every skill in the game"). The site lists its
//! skills at `GET /api/v1/skills` (`docs/api.md`, "Skills"). Updating the site is an app change
//! (`rails skills:sync`, a commit and a deploy), so the companion only compares and reports, with
//! text Darren can paste into an app issue. Nothing is sent.
//!
//! Skills are matched on the game's `id` (the site's `game_id`): a game `key` can change. The fields
//! compared are the ones `skills:sync` brings in step: name, game key, XP factor; and the ones it only
//! reports: a skill that moved school or category.

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};

/// One skill in the game-skills file.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameSkill {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub school: String,
    pub subcategory: String,
    pub xp_multiplier: f64,
}

/// The game-skills file. Its header fields are kept for the report.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameSkillsFile {
    #[serde(rename = "type")]
    pub file_type: Option<String>,
    pub api_version: Option<i64>,
    pub plugin_version: Option<String>,
    pub skills: Vec<GameSkill>,
}

/// Reads a game-skills file; `None` if it's another kind of file or isn't complete.
pub fn parse_file(bytes: &[u8]) -> Option<GameSkillsFile> {
    let file: GameSkillsFile = serde_json::from_slice(bytes).ok()?;
    (file.file_type.as_deref() == Some("gameSkills")).then_some(file)
}

/// One skill on the site (`GET /skills`).
#[derive(Debug, Clone, Deserialize)]
pub struct SiteSkill {
    pub key: String,
    pub name: String,
    pub category: String,
    pub school: String,
    #[serde(deserialize_with = "number")]
    pub xp_factor: f64,
    pub game_id: Option<i64>,
    pub game_key: Option<String>,
}

/// `GET /skills`'s answer: the skills, and the game school names the site renames.
#[derive(Debug, Clone, Deserialize)]
pub struct SiteSkills {
    pub skills: Vec<SiteSkill>,
    #[serde(default)]
    pub game_schools: HashMap<String, String>,
}

/// A number that may come as a JSON number or as text (`"1.5"`): Ruby can send a decimal as either.
fn number<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Number(n) => n.as_f64().ok_or_else(|| serde::de::Error::custom("not a number")),
        serde_json::Value::String(s) => s.trim().parse().map_err(serde::de::Error::custom),
        other => Err(serde::de::Error::custom(format!("expected a number, got {other}"))),
    }
}

/// A skill in a list of the report.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Line {
    pub game_id: Option<i64>,
    pub name: String,
    /// `category / school`, as the site would place it.
    pub place: String,
    /// What's different, one item per field: `name: "Quick Swapping" → "Efficient Swapping"`.
    pub details: Vec<String>,
}

/// The comparison.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub checked_at: String,
    pub file: String,
    pub plugin_version: Option<String>,
    pub api_version: Option<i64>,
    pub game_count: usize,
    pub site_count: usize,
    /// In the game, not on the site: `skills:sync` would add them.
    pub added: Vec<Line>,
    /// On both, with a different name, game key or XP factor: `skills:sync` would update them.
    pub changed: Vec<Line>,
    /// In another school or category in the game: `skills:sync` only reports these.
    pub moved: Vec<Line>,
    /// On the site with a game id the file doesn't have: gone from the game; `skills:sync` would remove them.
    pub removed: Vec<Line>,
    /// On the site with no game id: not linked to any game skill.
    pub unlinked: Vec<Line>,
    pub in_step: bool,
    /// Markdown for an issue in `sotalore/sotalore`, when not in step.
    pub issue_text: String,
}

/// Where the site would put a game skill: `(category, school)`, as `skills:sync` works it out
/// (the subcategory and school lowercased, the school renamed by `game_schools`).
fn place(game: &GameSkill, schools: &HashMap<String, String>) -> (String, String) {
    let school = game.school.to_lowercase();
    let school = schools.get(&school).cloned().unwrap_or(school);
    (game.subcategory.to_lowercase(), school)
}

/// Compares the game's skills with the site's.
pub fn compare(file: &GameSkillsFile, file_name: &str, site: &SiteSkills, checked_at: &str) -> Report {
    let by_game_id: HashMap<i64, &SiteSkill> = site.skills.iter().filter_map(|s| Some((s.game_id?, s))).collect();
    let game_ids: HashSet<i64> = file.skills.iter().map(|g| g.id).collect();
    let (mut added, mut changed, mut moved) = (vec![], vec![], vec![]);

    for game in &file.skills {
        let (category, school) = place(game, &site.game_schools);
        let Some(skill) = by_game_id.get(&game.id) else {
            added.push(Line {
                game_id: Some(game.id),
                name: game.name.clone(),
                place: format!("{category} / {school}"),
                details: vec![format!("key {}, XP factor {}", game.key, game.xp_multiplier)],
            });
            continue;
        };
        let mut details = vec![];
        if skill.name != game.name {
            details.push(format!("name: \"{}\" → \"{}\"", skill.name, game.name));
        }
        if skill.game_key.as_deref() != Some(game.key.as_str()) {
            details.push(format!("game key: {} → {}", skill.game_key.as_deref().unwrap_or("none"), game.key));
        }
        if (skill.xp_factor - game.xp_multiplier).abs() > 1e-9 {
            details.push(format!("XP factor: {} → {}", skill.xp_factor, game.xp_multiplier));
        }
        let line = |details| Line {
            game_id: Some(game.id),
            name: game.name.clone(),
            place: format!("{} / {}", skill.category, skill.school),
            details,
        };
        if !details.is_empty() {
            changed.push(line(details));
        }
        let mut move_details = vec![];
        if skill.school != school {
            move_details.push(format!("school: {} → {}", skill.school, school));
        }
        if skill.category != category {
            move_details.push(format!("category: {} → {}", skill.category, category));
        }
        if !move_details.is_empty() {
            moved.push(line(move_details));
        }
    }

    let site_line = |s: &SiteSkill, details: Vec<String>| Line {
        game_id: s.game_id,
        name: s.name.clone(),
        place: format!("{} / {}", s.category, s.school),
        details,
    };
    let removed: Vec<Line> = site
        .skills
        .iter()
        .filter(|s| s.game_id.is_some_and(|id| !game_ids.contains(&id)))
        .map(|s| site_line(s, vec![format!("site key {}", s.key)]))
        .collect();
    let unlinked: Vec<Line> =
        site.skills.iter().filter(|s| s.game_id.is_none()).map(|s| site_line(s, vec![format!("site key {}", s.key)])).collect();

    let mut report = Report {
        checked_at: checked_at.to_string(),
        file: file_name.to_string(),
        plugin_version: file.plugin_version.clone(),
        api_version: file.api_version,
        game_count: file.skills.len(),
        site_count: site.skills.len(),
        in_step: added.is_empty() && changed.is_empty() && moved.is_empty() && removed.is_empty() && unlinked.is_empty(),
        added,
        changed,
        moved,
        removed,
        unlinked,
        issue_text: String::new(),
    };
    if !report.in_step {
        report.issue_text = issue_text(&report);
    }
    report
}

/// The report as Markdown, for an issue in `sotalore/sotalore` (whose agent runs `skills:sync`).
fn issue_text(r: &Report) -> String {
    let mut text = format!(
        "The site's skills are out of step with the game. The companion compared `{}` (sl-admin {}, game API {}, \
         {} skills) with `GET /api/v1/skills` ({} skills) on {}, matching on the game's `id`.\n",
        r.file,
        r.plugin_version.as_deref().unwrap_or("?"),
        r.api_version.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
        r.game_count,
        r.site_count,
        r.checked_at,
    );
    let section = |text: &mut String, title: &str, lines: &[Line]| {
        if lines.is_empty() {
            return;
        }
        text.push_str(&format!("\n**{title}** ({})\n\n", lines.len()));
        for l in lines {
            let id = l.game_id.map(|id| format!(" (game id {id})")).unwrap_or_default();
            text.push_str(&format!("- {}{id}, {}: {}\n", l.name, l.place, l.details.join("; ")));
        }
    };
    section(&mut text, "New in the game (skills:sync adds them)", &r.added);
    section(&mut text, "Changed (skills:sync updates them)", &r.changed);
    section(&mut text, "Moved school or category (skills:sync only reports these)", &r.moved);
    section(&mut text, "Gone from the game (skills:sync removes them)", &r.removed);
    section(&mut text, "On the site with no game id", &r.unlinked);
    text.push_str(
        "\nTo bring the site in step: copy the new file into the app as its game-skills list and run `rails skills:sync`.\n",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(id: i64, key: &str, name: &str, school: &str, sub: &str, xp: f64) -> GameSkill {
        GameSkill {
            id,
            key: key.into(),
            name: name.into(),
            school: school.into(),
            subcategory: sub.into(),
            xp_multiplier: xp,
        }
    }

    fn site(_n: i64, game_id: Option<i64>, game_key: &str, name: &str, category: &str, school: &str, xp: f64) -> SiteSkill {
        SiteSkill {
            key: format!("{category}~{school}~{}", name.to_lowercase().replace(' ', "-")),
            name: name.into(),
            category: category.into(),
            school: school.into(),
            xp_factor: xp,
            game_id,
            game_key: Some(game_key.into()),
        }
    }

    fn file(skills: Vec<GameSkill>) -> GameSkillsFile {
        GameSkillsFile { file_type: Some("gameSkills".into()), api_version: Some(57), plugin_version: Some("0.15.0".into()), skills }
    }

    fn schools() -> HashMap<String, String> {
        HashMap::from([("polearms".into(), "polearm".into()), ("moon".into(), "lunar".into())])
    }

    #[test]
    fn in_step_when_everything_matches_after_the_school_renames() {
        let f = file(vec![
            game(12, "DoubleThrust", "Double Slash", "Blades", "Combat", 1.0),
            game(300, "Lunar1", "Moon Bolt", "Moon", "Magic", 1.5),
        ]);
        let s = SiteSkills {
            skills: vec![
                site(1, Some(12), "DoubleThrust", "Double Slash", "combat", "blades", 1.0),
                site(2, Some(300), "Lunar1", "Moon Bolt", "magic", "lunar", 1.5),
            ],
            game_schools: schools(),
        };
        let r = compare(&f, "sotalore-game-skills.json", &s, "2026-11-23 15:00:00");
        assert!(r.in_step, "{r:?}");
        assert!(r.issue_text.is_empty());
    }

    #[test]
    fn reports_each_kind_of_difference() {
        // The differences EXPORT-FORMAT.md lists between the first and second real files, and more.
        let f = file(vec![
            game(180, "QuickSwap", "Efficient Swapping", "Tactics", "Combat", 1.0), // renamed
            game(138, "Balance", "Balance", "Focus", "Magic", 1.0),                 // key changed
            game(16000, "Sail", "Sailing", "Seafaring", "Gathering", 2.0),          // new
            game(50, "Spear", "Spear", "Polearms", "Combat", 1.25),                 // XP changed
            game(60, "Thing", "Thing", "Blades", "Combat", 1.0),                    // moved school
        ]);
        let s = SiteSkills {
            skills: vec![
                site(1, Some(180), "QuickSwap", "Quick Swapping", "combat", "tactics", 1.0),
                site(2, Some(138), "Redirection", "Balance", "magic", "focus", 1.0),
                site(3, Some(50), "Spear", "Spear", "combat", "polearm", 1.0),
                site(4, Some(60), "Thing", "Thing", "combat", "bludgeons", 1.0),
                site(5, Some(3757), "LifeWard", "Life Warding", "magic", "life", 1.0), // gone
                site(6, None, "x", "Old Skill", "combat", "blades", 1.0),              // never linked
            ],
            game_schools: schools(),
        };
        let r = compare(&f, "sotalore-game-skills.json", &s, "2026-11-23 15:00:00");
        assert!(!r.in_step);
        assert_eq!(r.added.iter().map(|l| l.game_id).collect::<Vec<_>>(), vec![Some(16000)]);
        assert_eq!(r.added[0].place, "gathering / seafaring");
        let changed: Vec<_> = r.changed.iter().map(|l| (l.game_id.unwrap(), l.details.join("; "))).collect();
        assert_eq!(
            changed,
            vec![
                (180, "name: \"Quick Swapping\" → \"Efficient Swapping\"".to_string()),
                (138, "game key: Redirection → Balance".to_string()),
                (50, "XP factor: 1 → 1.25".to_string()),
            ]
        );
        assert_eq!(r.moved.len(), 1);
        assert_eq!(r.moved[0].details, vec!["school: bludgeons → blades"]);
        assert_eq!(r.removed.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), vec!["Life Warding"]);
        assert_eq!(r.unlinked.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), vec!["Old Skill"]);
        assert!(r.issue_text.contains("**New in the game (skills:sync adds them)** (1)"));
        assert!(r.issue_text.contains("- Sailing (game id 16000), gathering / seafaring: key Sail, XP factor 2"));
        assert!(r.issue_text.contains("rails skills:sync"));
    }

    #[test]
    fn reads_the_api_answer_with_xp_as_number_or_text() {
        let s: SiteSkills = serde_json::from_str(
            r#"{ "skills": [
                { "id": 1, "key": "combat~blades~double-slash", "name": "Double Slash", "activity": "adventuring",
                  "category": "combat", "school": "blades", "xp_factor": 1.0, "game_id": 12, "game_key": "DoubleThrust" },
                { "id": 2, "key": "k", "name": "N", "activity": "crafting", "category": "production",
                  "school": "x", "xp_factor": "0.5", "game_id": null, "game_key": null } ],
              "game_schools": { "polearms": "polearm", "moon": "lunar" } }"#,
        )
        .unwrap();
        assert_eq!(s.skills[1].xp_factor, 0.5);
        assert_eq!(s.game_schools["moon"], "lunar");
    }

    #[test]
    fn parses_only_game_skills_files() {
        let ok = br#"{"apiVersion":57,"plugin":"sl-admin","pluginVersion":"0.15.0","type":"gameSkills","count":1,"skills":[
            {"id":10,"key":"Thrust","name":"Thrust","schoolKey":"Blades","school":"Blades","subcategory":"Combat","xpMultiplier":1}]}"#;
        assert_eq!(parse_file(ok).unwrap().skills[0].id, 10);
        assert!(parse_file(br#"{"type":"recipes","skills":[]}"#).is_none());
    }

    #[test]
    fn the_real_file_parses() {
        // The regular client's file, if this machine has one (378 skills from sl-admin 0.15.0).
        let Some(appdata) = std::env::var_os("APPDATA") else { return };
        let path = std::path::PathBuf::from(appdata).join(r"Catnip Games\Shroud of the Avatar\Lua\sotalore-game-skills.json");
        let Ok(bytes) = std::fs::read(path) else { return };
        let f = parse_file(&bytes).expect("the real game-skills file parses");
        assert!(f.skills.len() > 300);
    }
}
