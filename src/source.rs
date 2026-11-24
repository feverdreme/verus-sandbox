//! Symbol index, visibility rules and interface stubs.

use crate::config::Experiment;
use anyhow::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Interface,
    Hidden,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum File {
    Task,
    Dependencies,
}

#[derive(Clone, Debug)]
pub struct Symbol {
    /// `name`, or `Type::method` for a method.
    pub name: String,
    /// For example "spec fn", "proof fn", "fn", "struct".
    pub kind: String,
    pub file: File,
    pub visibility: Visibility,
    /// 1-based inclusive line range in the real source.
    pub lines: (usize, usize),
    /// What the agent may see. Empty for hidden symbols.
    pub view: String,
}

pub struct Index {
    pub symbols: Vec<Symbol>,
    /// The agent-visible replacement for dependencies.rs.
    pub stubs: String,
}

impl Index {
    /// Fails if the config is invalid, including when anything visible mentions a hidden symbol.
    pub fn build(_experiment: &Experiment) -> Result<Index> {
        todo!()
    }

    /// Last path segments of hidden symbols, for example `method` for `Type::method`.
    pub fn hidden_names(&self) -> Vec<String> {
        todo!()
    }
}
