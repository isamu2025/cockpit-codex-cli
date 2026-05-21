use crate::account::{import_auth_file, Account, AccountSummary, ImportedAccount};
use crate::config::{ensure_data_dir, load_config, load_or_create_gateway_key, rotate_gateway_key, Config, GatewayKey};
use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Store {
    data_dir: PathBuf,
}

impl Store {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn init(&self) -> Result<()> {
        ensure_data_dir(&self.data_dir)?;
        let _ = self.config()?;
        let _ = self.gateway_key()?;
        Ok(())
    }

    pub fn config(&self) -> Result<Config> {
        load_config(&self.data_dir)
    }

    pub fn gateway_key(&self) -> Result<GatewayKey> {
        load_or_create_gateway_key(&self.data_dir)
    }

    pub fn rotate_gateway_key(&self) -> Result<GatewayKey> {
        rotate_gateway_key(&self.data_dir)
    }

    pub fn import_auth(
        &self,
        path: &Path,
        name: &str,
        email: Option<&str>,
    ) -> Result<ImportedAccount> {
        ensure_data_dir(&self.data_dir)?;
        let imported = import_auth_file(path, name, email)?;
        self.save_account(&imported.account)?;
        Ok(imported)
    }

    pub fn save_account(&self, account: &Account) -> Result<()> {
        ensure_data_dir(&self.data_dir)?;
        let path = self.account_path(&account.id);
        let text = serde_json::to_string_pretty(account)?;
        fs::write(&path, text).with_context(|| format!("write {}", path.display()))
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>> {
        ensure_data_dir(&self.data_dir)?;
        let mut accounts = Vec::new();
        for entry in fs::read_dir(self.data_dir.join("accounts"))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            let account: Account =
                serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
            accounts.push(account);
        }
        accounts.sort_by(|a, b| a.email.cmp(&b.email).then(a.name.cmp(&b.name)));
        Ok(accounts)
    }

    pub fn list_account_summaries(&self) -> Result<Vec<AccountSummary>> {
        Ok(self.list_accounts()?.into_iter().map(|account| account.summary()).collect())
    }

    pub fn remove_account(&self, id_or_email: &str) -> Result<Account> {
        let account = self
            .find_account(id_or_email)?
            .ok_or_else(|| anyhow!("account not found: {}", id_or_email))?;
        let path = self.account_path(&account.id);
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        Ok(account)
    }

    pub fn find_account(&self, id_or_email: &str) -> Result<Option<Account>> {
        let needle = id_or_email.trim();
        Ok(self.list_accounts()?.into_iter().find(|account| {
            account.id == needle
                || account.email.eq_ignore_ascii_case(needle)
                || account.name.eq_ignore_ascii_case(needle)
        }))
    }

    fn account_path(&self, id: &str) -> PathBuf {
        self.data_dir.join("accounts").join(format!("{}.json", id))
    }
}

