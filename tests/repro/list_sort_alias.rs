use beads::cli::ListArgs;
use beads::cli::commands::list;
use beads::config::CliOverrides;
use beads::output::OutputContext;

#[test]
fn test_list_sort_aliases_are_accepted() {
    let args = ListArgs {
        sort: Some("created".to_string()),
        ..Default::default()
    };
    let overrides = CliOverrides::default();
    let ctx = OutputContext::from_flags(false, false, true);

    // This should now SUCCEED
    let result = list::execute(&args, false, &overrides, &ctx);

    if let Err(e) = result {
        panic!("Expected Ok, got {e:?}");
    }
}
