#![feature(iterator_try_collect)]

fn main() {
    let repo = gix::open(".").unwrap();
    let head = repo.rev_parse_single("HEAD").unwrap();
    let main = repo.rev_parse_single("main").unwrap();
    let commits = repo
        .rev_walk([head])
        .all()
        .unwrap()
        .take_while(|res| res.as_ref().map(|info| info.id != main).unwrap_or(true))
        .try_collect::<Vec<_>>();
    println!("{commits:?}");
}

// fn main() {
//     let repo = gix::open(".").unwrap();
//     let head_commit_id = repo
//         .rev_parse_single("90ec80595c03e0b302c4becb003f184896e76de2")
//         .unwrap();
//     // let head_commit_info =
//     //     .next();

//     for info in repo
//         .rev_walk([head_commit_id])
//         .first_parent_only()
//         .all()
//         .unwrap()
//     {
//         println!("{:?}", info);
//     }
// }
