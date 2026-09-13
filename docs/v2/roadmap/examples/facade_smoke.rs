// Compiled and executed by crates/selene-db/tests/release_examples.rs.
// Uses only the stable facade; there is no lower-engine construction shortcut.
use selene_db::{CreatePolicy, Database, ObjectPath, SchemaPath, WriteSummary};

#[test]
fn named_graph_insert_and_query() -> selene_db::Result<()> {
    let database = Database::builder().build();
    let catalog = database.catalog();
    let schema = SchemaPath::regular("selene", "memory")?;
    catalog.create_schema(&schema, CreatePolicy::Strict)?;

    let graph = ObjectPath::regular("selene", "memory", "episodes")?;
    catalog.create_graph(&graph, None, CreatePolicy::Strict)?;
    let session = database.session(&graph)?;

    let written = session.execute("INSERT (:Person { name: 'Ada' })")?;
    assert_eq!(written.write_summary(), Some(WriteSummary::new(1, None)));

    let rows = session.execute("MATCH (n:Person) RETURN n")?;
    assert_eq!(rows.row_count(), Some(1));
    Ok(())
}
