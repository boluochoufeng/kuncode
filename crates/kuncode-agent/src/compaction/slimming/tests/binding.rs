use super::{FixedMarkerCounter, artifact_source, fixture_groups, location};
use crate::{
    compaction::{protocol::select_protected_recent_tail, slimming::slim_tool_results},
    session_store::Seq,
};

#[tokio::test]
async fn binds_projected_groups_to_the_exact_artifact_pass() {
    // Given: two value-equal artifact capabilities from distinct spill passes.
    let groups = fixture_groups();
    let protected = select_protected_recent_tail(&groups, 0, |_| 1)
        .expect("fixture should have a protected tail");
    let source = artifact_source(&groups, &[(location(0), "old", 1_000, Some(Seq::new(2)))]);
    let lookalike = artifact_source(&groups, &[(location(0), "old", 1_000, Some(Seq::new(2)))]);

    // When: projection succeeds against only the first capability.
    let result = slim_tool_results(
        &source,
        &protected,
        &[location(0)],
        &FixedMarkerCounter::new(100),
    )
    .await
    .expect("authorized projection should succeed");

    // Then: the projection retains only the exact authorizing capability.
    assert!(std::ptr::eq(result.source(), &source));
    assert!(!std::ptr::eq(result.source(), &lookalike));
}
