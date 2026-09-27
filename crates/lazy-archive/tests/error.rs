use lazy_archive::{Error, Result};

#[test]
fn error_types_are_usable_by_readers() {
    let result: Result<()> = Err(Error::NotFound("missing.txt".to_owned()));

    let error = match result {
        Ok(()) => panic!("expected an error"),
        Err(error) => error,
    };

    assert_eq!(
        error.to_string(),
        "archive entry was not found: missing.txt"
    );
}
