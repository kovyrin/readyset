use std::{fmt, str};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct Table {
    pub name: String,
    pub alias: Option<String>,
    pub schema: Option<String>,
}

impl fmt::Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(ref schema) = self.schema {
            write!(f, "`{}`.", schema)?;
        }
        write!(f, "`{}`", self.name)?;
        if let Some(ref alias) = self.alias {
            write!(f, " AS `{}`", alias)?;
        }
        Ok(())
    }
}

impl<'a> From<&'a str> for Table {
    fn from(t: &str) -> Table {
        Table {
            name: String::from(t),
            alias: None,
            schema: None,
        }
    }
}
impl<'a> From<(&'a str, &'a str)> for Table {
    fn from(t: (&str, &str)) -> Table {
        Table {
            name: String::from(t.1),
            alias: None,
            schema: Some(String::from(t.0)),
        }
    }
}
