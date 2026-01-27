use std::borrow::Cow;
use std::{collections::HashMap, fmt::Debug};

use axum::extract::rejection::JsonRejection;
use axum::response::Response;
use axum::{Json, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tracing::info;
use validator::{ValidationErrors, ValidationErrorsKind};

#[derive(Debug, Deserialize, Serialize)]
pub struct ApiError {
    pub errors: HashMap<String, Vec<String>>,
}

impl ApiError {
    pub fn new(error: String) -> Self {
        let mut error_map: HashMap<String, Vec<String>> = HashMap::new();
        error_map.insert("message".to_owned(), vec![error]);
        Self { errors: error_map }
    }
}

// just a name alias but its literally just a normal result with some extra functionality to
// return status codes based on err
pub type AppResult<T> = Result<T, Error>;

pub type ErrorMap = HashMap<Cow<'static, str>, Vec<Cow<'static, str>>>;

// add any errors that are needed here
#[derive(Error, Debug)]
pub enum Error {
    #[error("authentication is required to access this resource")]
    Unauthorized,
    #[error("username or password is incorrect")]
    InvalidLoginAttmpt,
    #[error("user does not have privilege to access this resource")]
    Forbidden,
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    ApplicationStartup(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("unexpected error has occurred")]
    InternalServerError,
    #[error("{0}")]
    InternalServerErrorWithContext(String),
    #[error("{0}")]
    ObjectConflict(String),
    #[error("unprocessable request has occurred")]
    UnprocessableEntity { errors: ErrorMap },
    #[error("{message}")]
    TooManyRequests { message: String, retry_after: u64 },
    #[error(transparent)]
    ValidationError(#[from] ValidationErrors),
    #[error(transparent)]
    AxumJsonRejection(#[from] JsonRejection),
    #[error(transparent)]
    AnyhowError(#[from] anyhow::Error),
}

impl Error {
    pub fn unprocessable_entity(errors: ValidationErrors) -> Response {
        let mut validation_errors = ErrorMap::new();

        // unroll struct
        for (field_property, error_kind) in errors.into_errors() {
            if let ValidationErrorsKind::Field(field_meta) = error_kind.clone() {
                for error in field_meta.into_iter() {
                    validation_errors
                        .entry(field_property.clone())
                        .or_default()
                        .push(error.message.unwrap_or_else(|| {
                            // required validators contain None for their message, assume a default response
                            let params: Vec<Cow<'static, str>> = error
                                .params
                                .iter()
                                .filter(|(key, _value)| *key != "value")
                                .map(|(key, value)| {
                                    Cow::from(format!("{} value is {}", key, value))
                                })
                                .collect();

                            if !params.is_empty() {
                                Cow::from(params.join(", "))
                            } else {
                                Cow::from(format!("{} is required", field_property))
                            }
                        }))
                }
            }
            // structs may contain validators on themselves, roll through first-depth validators
            if let ValidationErrorsKind::Struct(meta) = error_kind.clone() {
                // on structs with validation errors, roll through each of the structs properties to build a list of errors
                for (struct_property, struct_error_kind) in meta.into_errors() {
                    if let ValidationErrorsKind::Field(field_meta) = struct_error_kind {
                        for error in field_meta.into_iter() {
                            validation_errors
                                .entry(struct_property.clone())
                                .or_default()
                                .push(error.message.unwrap_or_else(|| {
                                    // required validators contain None for their message, assume a default response
                                    Cow::from(format!("{} is required", struct_property))
                                }));
                        }
                    }
                }
            }
        }

        let body = Json(json!({
            "errors": validation_errors,
        }));

        (StatusCode::BAD_REQUEST, body).into_response()
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        info!("{:#?}", self);
        if let Self::ValidationError(e) = self {
            return Self::unprocessable_entity(e);
        }

        // handle TooManyRequests separately to include Retry-After header
        if let Self::TooManyRequests {
            message,
            retry_after,
        } = self
        {
            let body = Json(json!({
                "errors": {
                    "message": [message]
                },
                "retry_after": retry_after
            }));
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(axum::http::header::RETRY_AFTER, retry_after.to_string())],
                body,
            )
                .into_response();
        }

        let (status, error_message) = match self {
            Self::InternalServerErrorWithContext(err) => (StatusCode::INTERNAL_SERVER_ERROR, err),
            Self::NotFound(err) => (StatusCode::NOT_FOUND, err),
            Self::ObjectConflict(err) => (StatusCode::CONFLICT, err),
            Self::InvalidLoginAttmpt => (
                StatusCode::BAD_REQUEST,
                Self::InvalidLoginAttmpt.to_string(),
            ),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, Self::Unauthorized.to_string()),
            Self::Forbidden => (StatusCode::FORBIDDEN, Self::Forbidden.to_string()),
            Self::AxumJsonRejection(err) => (StatusCode::BAD_REQUEST, err.body_text()),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                String::from("An internal error occured"),
            ),
        };

        let body = Json(ApiError::new(error_message));

        (status, body).into_response()
    }
}
