use anyhow::Result;
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct SheetWrite {
    pub row: usize,
    pub date: Option<String>,
    pub hours: f64,
}

#[async_trait]
pub trait SheetProvider: Send + Sync {
    async fn get_all_values(&self, sheet_key: &str, worksheet: &str) -> Result<Vec<Vec<String>>>;
    async fn write_changes(
        &self,
        sheet_key: &str,
        worksheet: &str,
        changes: Vec<SheetWrite>,
    ) -> Result<()>;
}
