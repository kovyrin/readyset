use std::convert::TryFrom;

use noria_data::DataType;

use crate::Backend;

pub async fn create_users(backend: &mut Backend) {
    // username varchar(1024),
    // email varchar(1024),
    // name varchar(1024),
    // affiliation varchar(1024),
    // acm_number varchar(1024),
    // level varchar(12): one of "chair", "pc", "normal"
    let data: Vec<Vec<&str>> = vec![
        vec![
            "kohler",
            "kohler@seas.harvard.edu",
            "Eddie Kohler",
            "Harvard University",
            "0",
            "chair",
        ],
        vec![
            "rtm",
            "rtm@csail.mit.edu",
            "Robert Morris",
            "MIT CSAIL",
            "0",
            "pc",
        ],
        vec![
            "malte",
            "malte@csail.mit.edu",
            "Malte Schwarzkopf",
            "MIT CSAIL",
            "0",
            "normal",
        ],
        vec![
            "lara",
            "larat@mit.edu",
            "Lara Timbo",
            "MIT CSAIL",
            "0",
            "normal",
        ],
    ];
    let users: Vec<Vec<DataType>> = data
        .into_iter()
        .map(|v| {
            v.into_iter()
                .map(|e| DataType::try_from(e).unwrap())
                .collect::<Vec<DataType>>()
        })
        .collect();

    backend
        .g
        .table("UserProfile")
        .await
        .unwrap()
        .perform_all(users)
        .await
        .unwrap();
}

pub async fn create_papers(backend: &mut Backend) {
    // Paper
    // id int,
    // author varchar(1024),
    // accepted tinyint(1),
    let papers: Vec<Vec<DataType>> = vec![
        vec![
            1.into(),
            DataType::try_from("malte").unwrap(),
            DataType::try_from("0").unwrap(),
        ],
        vec![
            2.into(),
            DataType::try_from("lara").unwrap(),
            DataType::try_from("0").unwrap(),
        ],
        vec![
            3.into(),
            DataType::try_from("malte").unwrap(),
            DataType::try_from("0").unwrap(),
        ],
    ];

    // PaperVersion
    // paper int,
    // title varchar(1024),
    // contents varchar(1024),
    // abstract text,
    // time datetime DEFAULT CURRENT_TIMESTAMP,
    let paper_versions: Vec<Vec<DataType>> = vec![
        vec![
            1.into(),
            DataType::try_from("Why Soup is Awesome").unwrap(),
            DataType::try_from("Text").unwrap(),
            DataType::try_from("Soup is tasty.").unwrap(),
            DataType::try_from("0").unwrap(),
        ],
        vec![
            2.into(),
            DataType::try_from("Is Soup Tasty?").unwrap(),
            DataType::try_from("Text").unwrap(),
            DataType::try_from("Maybe.").unwrap(),
            DataType::try_from("0").unwrap(),
        ],
        vec![
            3.into(),
            DataType::try_from("How To Cook Soup").unwrap(),
            DataType::try_from("Text").unwrap(),
            DataType::try_from("Make it tasty.").unwrap(),
            DataType::try_from("0").unwrap(),
        ],
    ];

    backend
        .g
        .table("Paper")
        .await
        .unwrap()
        .perform_all(papers)
        .await
        .unwrap();
    backend
        .g
        .table("PaperVersion")
        .await
        .unwrap()
        .perform_all(paper_versions)
        .await
        .unwrap();
}

pub async fn dump_papers(backend: &mut Backend, user: &str) {
    let mut get = backend
        .g
        .view(&format!("PaperList_u{}", user))
        .await
        .unwrap();

    println!("{:?}", get.lookup(&[0.into()], true).await);
}

pub async fn dump_all_papers(backend: &mut Backend) {
    let mut get = backend.g.view("PaperList").await.unwrap();

    println!("{:?}", get.lookup(&[0.into()], true).await);
}
