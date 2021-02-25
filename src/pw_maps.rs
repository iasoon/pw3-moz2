use std::{collections::{HashMap, HashSet}, fs::File, io::Read, path::Path};
use std::fs;
use anyhow::{Context, Error, anyhow};
use crate::PwMapData;

use planetwars_rules::config::Map as PwMap;

pub fn build_map_index(dir_path: &Path) -> Result<HashMap<String, PwMapData>, Error>{
    let mut maps = HashMap::new();
    
    for entry in fs::read_dir(dir_path)? {
        let entry = entry?;
        let path = entry.path();
    
        if !entry.file_type()?.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }

        let map_data = get_map_data(&path)
            .with_context(|| format!("failed to read map file {:?}", path.into_os_string()))?;
        maps.insert(map_data.name.clone(), map_data);
    }
    return Ok(maps);
}

fn get_map_data(path: &Path) -> Result<PwMapData, Error> {
    let map_name = path.file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid map name"))?;
    let mut file = File::open(path)?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let pw_map: PwMap = serde_json::from_str(&buf)?;
    let players: HashSet<usize> = pw_map.planets
        .iter()
        .filter_map(|p| p.owner.clone())
        .collect();
    
    return Ok(PwMapData {
        name: map_name.to_string(),
        max_players: players.len(),
    })
}