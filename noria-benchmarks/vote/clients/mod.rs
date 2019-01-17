use clap;
use tokio::prelude::*;

#[derive(Copy, Clone, Debug)]
pub(crate) struct Parameters {
    pub(crate) prime: bool,
    pub(crate) articles: usize,
}

pub(crate) enum Operation {
    Read,
    Write,
}

pub(crate) struct Request {
    pub ids: Vec<i32>,
    pub op: Operation,
}

pub(crate) trait VoteClient
where
    Self: Sized,
{
    type Future: Future<Item = Self, Error = failure::Error> + Send + 'static;
    fn new(
        ex: tokio::runtime::TaskExecutor,
        params: Parameters,
        args: clap::ArgMatches,
    ) -> <Self as VoteClient>::Future;
}

//pub(crate) mod hybrid;
pub(crate) mod localsoup;
//pub(crate) mod memcached;
//pub(crate) mod mssql;
//pub(crate) mod mysql;
//pub(crate) mod netsoup;
