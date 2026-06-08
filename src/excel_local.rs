use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::PathBuf;

use crate::provider::{SheetProvider, SheetWrite};

pub struct ExcelLocalProvider;

fn resolve_path(sheet_key: &str) -> PathBuf {
    if sheet_key.starts_with('~') {
        let home = dirs::home_dir().unwrap_or_default();
        home.join(sheet_key.trim_start_matches("~/"))
    } else {
        PathBuf::from(sheet_key)
    }
}

fn open_or_create_workbook(path: &PathBuf) -> Result<umya_spreadsheet::Spreadsheet> {
    if path.exists() {
        umya_spreadsheet::reader::xlsx::read(path)
            .with_context(|| format!("Failed to read Excel file: {}", path.display()))
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }
        Ok(umya_spreadsheet::new_file())
    }
}

fn ensure_worksheet<'a>(
    book: &'a mut umya_spreadsheet::Spreadsheet,
    worksheet: &str,
) -> &'a mut umya_spreadsheet::Worksheet {
    if book.get_sheet_by_name(worksheet).is_none() {
        book.new_sheet(worksheet).expect("Failed to create sheet");
    }
    book.get_sheet_by_name_mut(worksheet)
        .expect("Sheet must exist after creation")
}

fn sheet_to_rows(sheet: &umya_spreadsheet::Worksheet) -> Vec<Vec<String>> {
    let max_row = sheet.get_highest_row();
    (1..=max_row)
        .map(|r| {
            let a = sheet
                .get_cell((1u32, r))
                .map(|c| c.get_value().to_string())
                .unwrap_or_default();
            let b = sheet
                .get_cell((2u32, r))
                .map(|c| c.get_value().to_string())
                .unwrap_or_default();
            vec![a, b]
        })
        .collect()
}

#[async_trait]
impl SheetProvider for ExcelLocalProvider {
    async fn get_all_values(&self, sheet_key: &str, worksheet: &str) -> Result<Vec<Vec<String>>> {
        let path = resolve_path(sheet_key);
        let worksheet = worksheet.to_string();
        tokio::task::spawn_blocking(move || {
            if !path.exists() {
                return Ok(vec![]);
            }
            let book = umya_spreadsheet::reader::xlsx::read(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            match book.get_sheet_by_name(&worksheet) {
                Some(sheet) => Ok(sheet_to_rows(sheet)),
                None => Ok(vec![]),
            }
        })
        .await?
    }

    async fn write_changes(
        &self,
        sheet_key: &str,
        worksheet: &str,
        changes: Vec<SheetWrite>,
    ) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let path = resolve_path(sheet_key);
        let worksheet = worksheet.to_string();
        tokio::task::spawn_blocking(move || {
            let mut book = open_or_create_workbook(&path)?;
            {
                let sheet = ensure_worksheet(&mut book, &worksheet);
                for change in changes {
                    if let Some(date) = change.date {
                        sheet
                            .get_cell_mut((1u32, change.row as u32))
                            .set_value_string(date);
                    }
                    sheet
                        .get_cell_mut((2u32, change.row as u32))
                        .set_value_number(change.hours);
                }
            }
            umya_spreadsheet::writer::xlsx::write(&book, &path)
                .with_context(|| format!("Failed to write {}", path.display()))
        })
        .await?
    }
}
