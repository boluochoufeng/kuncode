use kuncode_core::{ToolCapability, ToolEffect};
use kuncode_tools::builtin_tools;

#[test]
fn builtin_descriptors_match_phase2_truth_table() {
    use ToolCapability::{Edit, Explore, Verify};
    use ToolEffect::{ExecuteProcess, ReadWorkspace, WriteWorkspace};

    let actual: Vec<_> = builtin_tools()
        .into_iter()
        .map(|tool| {
            let descriptor = tool.descriptor();
            (descriptor.name.clone(), descriptor.effects.clone(), descriptor.default_capabilities.clone())
        })
        .collect();
    let expected = vec![
        ("read_file".to_owned(), vec![ReadWorkspace], vec![Explore, Edit]),
        ("search".to_owned(), vec![ReadWorkspace], vec![Explore, Edit]),
        ("write_file".to_owned(), vec![WriteWorkspace], vec![Edit]),
        ("apply_patch".to_owned(), vec![WriteWorkspace], vec![Edit]),
        ("exec_argv".to_owned(), vec![ExecuteProcess], vec![Verify, Edit]),
        ("git_status".to_owned(), vec![ReadWorkspace], vec![Explore, Verify]),
        ("git_diff".to_owned(), vec![ReadWorkspace], vec![Explore, Verify]),
    ];

    assert_eq!(actual, expected);
}
