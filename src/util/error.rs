use std::borrow::Cow;
use std::error::Error;
use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StrError(Cow<'static, str>);

impl StrError {
    pub fn new(v: impl Into<Cow<'static, str>>) -> Self {
        Self(v.into())
    }
}

impl Display for StrError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_ref())
    }
}

impl Error for StrError {}
