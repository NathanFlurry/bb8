use tokio;
use tokio_postgres;

use bb8::Pool;
use bb8_postgres::PostgresConnectionManager;
use futures::{
    future::{err, lazy, Either},
    Future, Stream,
};

// Select some static data from a Postgres DB
//
// The simplest way to start the db is using Docker:
// docker run --name gotham-middleware-postgres -e POSTGRES_PASSWORD=mysecretpassword -p 5432:5432 -d postgres
fn main() {
    let pg_mgr = PostgresConnectionManager::new_from_stringlike(
        "postgresql://postgres:mysecretpassword@localhost:5432",
        tokio_postgres::NoTls,
    )
    .unwrap();

    tokio::run(lazy(|| {
        Pool::builder()
            .build(pg_mgr)
            .map_err(|e| bb8::RunError::User(e))
            .and_then(|pool| {
                pool.run(|mut connection| {
                    connection
                        .simple_query("BEGIN")
                        .for_each(|_| Ok(()))
                        .then(|r| match r {
                            Ok(_) => Ok(connection),
                            Err(e) => Err((e, connection)),
                        })
                        .and_then(|mut connection| {
                            connection.prepare("SELECT 1").then(move |r| match r {
                                Ok(select) => {
                                    let f = connection
                                        .query(&select, &[])
                                        .for_each(|row| {
                                            println!("result: {}", row.get::<usize, i32>(0));
                                            Ok(())
                                        })
                                        .then(move |r| match r {
                                            Ok(_) => Ok(connection),
                                            Err(e) => Err((e, connection)),
                                        });
                                    Either::A(f)
                                }
                                Err(e) => Either::B(err((e, connection))),
                            })
                        })
                        .and_then(|mut connection| {
                            connection
                                .simple_query("COMMIT")
                                .for_each(|_| Ok(()))
                                .then(|r| match r {
                                    Ok(_) => Ok(((), connection)),
                                    Err(e) => Err((e, connection)),
                                })
                        })
                        .or_else(|(e, mut connection)| {
                            connection
                                .simple_query("ROLLBACK")
                                .for_each(|_| Ok(()))
                                .then(|_| Err((e, connection)))
                        })
                })
            })
            .map_err(|e| panic!("{:?}", e))
    }));
}
