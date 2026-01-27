use axum::http::Request;
use axum::{
    Json,
    body::Body,
    extract::{FromRequest, rejection::JsonRejection},
};
use serde::de::DeserializeOwned;
use validator::Validate;

use crate::server::error::Error;

pub struct ValidationExtractor<T>(pub T);

impl<T, S> FromRequest<S> for ValidationExtractor<T>
where
    T: DeserializeOwned + Validate,
    S: Send + Sync,
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
{
    type Rejection = Error;

    async fn from_request(req: Request<Body>, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = Json::<T>::from_request(req, state).await?;
        value.validate()?;
        Ok(ValidationExtractor(value))
    }
}
