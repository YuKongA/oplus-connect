use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use gpui_kit::{AssetSource, SharedString, assets::AllAssets as KitAssets};

pub struct LocalAssets {
    base_dirs: Vec<PathBuf>,
}

fn asset_directories() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        dirs.push(dir.join("../share/oplus-connect/assets"));
        for parent in dir.ancestors().take(6) {
            dirs.push(parent.join("assets"));
            dirs.push(parent.join("oplus-connect-gui/assets"));
        }
    }
    dirs.push(PathBuf::from("assets"));
    dirs.push(PathBuf::from("oplus-connect-gui/assets"));
    dirs
}

pub fn asset_path(rel_path: &str) -> PathBuf {
    let clean = rel_path.trim_start_matches('/');
    asset_directories()
        .into_iter()
        .map(|dir| dir.join(clean))
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("assets").join(clean))
}

impl LocalAssets {
    pub fn new() -> Self {
        Self {
            base_dirs: asset_directories(),
        }
    }

    fn find_file(&self, path: &str) -> Option<PathBuf> {
        let direct = std::path::Path::new(path);
        if direct.is_file() {
            return Some(direct.to_path_buf());
        }
        let cleaned = path.trim_start_matches('/');
        for base in &self.base_dirs {
            let candidate = base.join(cleaned);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }

    fn find_dir(&self, path: &str) -> Option<PathBuf> {
        let direct = std::path::Path::new(path);
        if direct.is_dir() {
            return Some(direct.to_path_buf());
        }
        let cleaned = path.trim_start_matches('/');
        for base in &self.base_dirs {
            let candidate = base.join(cleaned);
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
        None
    }
}

impl Default for LocalAssets {
    fn default() -> Self {
        Self::new()
    }
}

impl AssetSource for LocalAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(file_path) = self.find_file(path) {
            let bytes = fs::read(file_path)?;
            // Keep `currentColor` intact so GPUI can apply each icon's explicit tint.
            return Ok(Some(std::borrow::Cow::Owned(bytes)));
        }

        // Native Kit assets are embedded; missing optional paths remain `None`.
        Ok(KitAssets::get(path).map(|asset| asset.data))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut seen = HashSet::new();
        let mut list = Vec::new();

        if let Some(dir_path) = self.find_dir(path)
            && let Ok(entries) = fs::read_dir(dir_path)
        {
            for entry in entries.flatten() {
                if let Ok(name) = entry.file_name().into_string() {
                    let key = format!("{}/{}", path.trim_end_matches('/'), name);
                    seen.insert(key.trim_start_matches('/').to_owned());
                    list.push(SharedString::from(name));
                }
            }
        }

        // Keep local entries first, so a custom path remains the preferred
        // listing when it has the same name as a bundled icon.
        for asset in KitAssets.list(path)? {
            if seen.insert(asset.to_string()) {
                list.push(asset);
            }
        }

        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kit_icons_and_missing_assets() {
        let assets = LocalAssets::new();
        let path = "icons/search.svg";
        assert_eq!(assets.load(path).unwrap(), KitAssets.load(path).unwrap());
        assert!(
            assets
                .list("icons/")
                .unwrap()
                .iter()
                .any(|name| name.as_ref() == path)
        );
        assert!(assets.load("missing/optional.svg").unwrap().is_none());
    }

    #[test]
    fn test_local_assets_override_kit_and_keep_local_listing_names() {
        let root = std::env::temp_dir().join(format!("oplus-assets-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("icons")).unwrap();
        fs::write(root.join("icons/search.svg"), b"local icon").unwrap();
        let assets = LocalAssets {
            base_dirs: vec![root.clone()],
        };
        let loaded = assets.load("icons/search.svg").unwrap().unwrap();
        let listed = assets.list("icons/").unwrap();
        fs::remove_dir_all(root).unwrap();

        assert_eq!(loaded.as_ref(), b"local icon");
        assert!(listed.iter().any(|name| name.as_ref() == "search.svg"));
        assert!(
            !listed
                .iter()
                .any(|name| name.as_ref() == "icons/search.svg")
        );
        assert!(listed.iter().any(|name| name.as_ref() == "icons/check.svg"));
    }

    #[test]
    fn absolute_asset_path_is_loaded() {
        let dir = std::env::temp_dir().join(format!("oplus-assets-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("icon.svg");
        fs::write(&path, b"<svg/>").unwrap();
        let bytes = LocalAssets::new().load(path.to_str().unwrap()).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(bytes.unwrap().as_ref(), b"<svg/>");
    }

    #[test]
    fn full_kit_icon_catalog_is_reachable() {
        let assets = LocalAssets::new();
        for path in [
            "icons/clipboard.svg",
            "icons/bell.svg",
            "icons/qr-code.svg",
            "icons/settings.svg",
            "icons/copy.svg",
            "icons/trash.svg",
            "icons/star.svg",
            "icons/star-fill.svg",
            "icons/x.svg",
            "icons/smartphone.svg",
            "icons/download.svg",
        ] {
            let loaded = assets.load(path).expect("asset load");
            assert!(loaded.is_some(), "missing kit icon {path}");
        }
    }
}
