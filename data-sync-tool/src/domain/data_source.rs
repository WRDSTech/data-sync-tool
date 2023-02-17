// Data Source Domain Object Definition

use chrono::prelude::*;
use std::collections::HashMap;
use fake::{Dummy, Fake, Faker};

#[derive(Debug, Dummy, PartialEq, Eq, Clone)]
#[readonly::make]
pub struct DataSource {
    #[dummy(faker = "UUIDv4")]
    pub id: Uuid,

    #[dummy(faker = "UUID4()")]
    pub name: String,
    pub description: String,
    pub api_key: String,
    pub create_date: DateTime<Local>,
    pub last_update: Option<DateTime<Local>>,
    pub update_successful: Option<bool>,
    pub datasets: HashMap<String, Dataset>
}

impl DataSource {
    pub fn new(id: &str, name: &str, description: &str, 
               api_key: &str, create_date: DateTime,
               last_update: Option<DateTime>, update_successful: Option<bool>,
               datasets: HashMap<String, Dataset>) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            description: description.to_string(),
            api_key: description.to_string(),
            create_date,
            last_update,
            update_successful,
            datasets
        }  
    }
}

#[cfg(test)]
mod test {
    use super::*;
}