use anyhow::Result;
use serde_json;
use serde_yaml;
use std::fs;
use std::path::Path;

use crate::types::PartitionRegistration;

pub struct ConfigExporter;

impl ConfigExporter {
    pub fn export_to_yaml(registrations: &[PartitionRegistration], path: &Path) -> Result<()> {
        let yaml = serde_yaml::to_string(registrations)?;
        fs::write(path, yaml)?;
        Ok(())
    }

    pub fn export_to_json(registrations: &[PartitionRegistration], path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(registrations)?;
        fs::write(path, json)?;
        Ok(())
    }

    pub fn import_from_yaml(path: &Path) -> Result<Vec<PartitionRegistration>> {
        let contents = fs::read_to_string(path)?;
        let registrations = serde_yaml::from_str(&contents)?;
        Ok(registrations)
    }

    pub fn import_from_json(path: &Path) -> Result<Vec<PartitionRegistration>> {
        let contents = fs::read_to_string(path)?;
        let registrations = serde_json::from_str(&contents)?;
        Ok(registrations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MigrationConfig, PartitionKey, PartitionStrategy};
    use tempfile::NamedTempFile;

    #[test]
    fn test_export_yaml() {
        let registrations = vec![];
        let temp_file = NamedTempFile::new().unwrap();
        let result = ConfigExporter::export_to_yaml(&registrations, temp_file.path());
        assert!(result.is_ok());
    }

    #[test]
    fn test_export_json() {
        let registrations = vec![];
        let temp_file = NamedTempFile::new().unwrap();
        let result = ConfigExporter::export_to_json(&registrations, temp_file.path());
        assert!(result.is_ok());
    }
}
