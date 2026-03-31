use std::collections::HashMap;

use super::types::{PlayerMetadata, ScriptMetadata, TeamMetadata};

pub(super) fn parse_game_script(
    script: &str,
) -> Result<ScriptMetadata, Box<dyn std::error::Error>> {
    let mut meta = ScriptMetadata::default();
    let mut pending_section: Option<String> = None;
    let mut stack: Vec<String> = Vec::new();
    let mut players: HashMap<u32, PlayerMetadata> = HashMap::new();
    let mut teams: HashMap<u32, TeamMetadata> = HashMap::new();

    for raw_line in script.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            pending_section = Some(line[1..line.len() - 1].to_string());
            continue;
        }

        if line == "{" {
            if let Some(section) = pending_section.take() {
                stack.push(section);
            }
            continue;
        }

        if line == "}" {
            stack.pop();
            continue;
        }

        let Some((key, value)) = parse_script_assignment(line) else {
            continue;
        };

        match stack.as_slice() {
            [game] if game == "game" => match key.as_str() {
                "mapname" => meta.map_name = Some(value),
                "gametype" => meta.game_name = Some(value),
                "zksearchtag" => meta.zksearchtag = Some(value),
                _ => {}
            },
            [game, section] if game == "game" && section.starts_with("player") => {
                let player_id = section["player".len()..].parse::<u32>()?;
                let player = players.entry(player_id).or_insert(PlayerMetadata {
                    player_id,
                    name: None,
                    team: None,
                    spectator: false,
                    elo: None,
                    lobby_id: None,
                    country_code: None,
                    clan: None,
                    level: None,
                });

                match key.as_str() {
                    "name" => player.name = Some(value),
                    "team" => player.team = value.parse().ok(),
                    "spectator" => player.spectator = value == "1",
                    "elo" => player.elo = value.parse().ok(),
                    "lobbyid" => player.lobby_id = value.parse().ok(),
                    "countrycode" => player.country_code = Some(value),
                    "clan" => player.clan = Some(value),
                    "level" => player.level = value.parse().ok(),
                    _ => {}
                }
            }
            [game, section] if game == "game" && section.starts_with("team") => {
                let team_id = section["team".len()..].parse::<u32>()?;
                let team = teams.entry(team_id).or_insert(TeamMetadata {
                    team_id,
                    allyteam: None,
                    teamleader: None,
                    handicap: None,
                });

                match key.as_str() {
                    "allyteam" => team.allyteam = value.parse().ok(),
                    "teamleader" => team.teamleader = value.parse().ok(),
                    "handicap" => team.handicap = value.parse().ok(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let mut player_values: Vec<_> = players.into_values().collect();
    player_values.sort_by_key(|player| player.player_id);
    meta.players = player_values;

    let mut team_values: Vec<_> = teams.into_values().collect();
    team_values.sort_by_key(|team| team.team_id);
    meta.teams = team_values;
    Ok(meta)
}

fn parse_script_assignment(line: &str) -> Option<(String, String)> {
    let line = line.strip_suffix(';').unwrap_or(line);
    let (key, value) = line.split_once('=')?;
    Some((key.trim().to_string(), value.trim().to_string()))
}
