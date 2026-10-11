#![no_std]

use conflaguration::ResolveFromSource;

#[derive(conflaguration_derive::BorrowedSettings)]
pub struct Config<'source> {
    pub workers: usize,
    pub color: &'source str,
}

pub fn resolve<'source, Source>(source: &'source Source) -> Result<Config<'source>, conflaguration::ResolveError>
where
    Source: conflaguration::SettingsSource + ?Sized,
{
    Config::resolve_from(source)
}
