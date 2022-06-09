use actix_identity::Identity;
use serde::{Deserialize, Serialize};

use crate::api::error::ApiError;
use crate::db::collections::{
    create_collection_item, get_collection_item, get_collection_items_paginated,
};

use crate::db::Pool;

use actix_web::web::Data;
use actix_web::{web, HttpRequest, HttpResponse};

use chrono::NaiveDateTime;
use reqwest::Client;

use crate::db;
use crate::db::error::DbError;
use crate::db::model::{CollectionAndDocumentQuery, UserQuery};
use crate::db::users::get_user;

use super::common::{get_document_metadata, Sorting};

#[derive(Deserialize)]
pub struct CollectionsQueryParams {
    pub q: Option<String>,
    pub sort: Option<Sorting>,
    pub url: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[derive(Serialize, Deserialize)]
struct CollectionParent {
    uri: String,
    title: String,
}

#[derive(Serialize)]
struct CollectionItem {
    id: i64,
    url: String,
    title: String,
    notes: Option<String>,
    parents: Vec<CollectionParent>,
    created: NaiveDateTime,
}

impl From<db::model::CollectionParent> for CollectionParent {
    fn from(parent: db::model::CollectionParent) -> Self {
        CollectionParent {
            uri: parent.uri,
            title: parent.title,
        }
    }
}

#[derive(Serialize)]
pub struct CollectionsResponse {
    items: Vec<CollectionItem>,
    csrfmiddlewaretoken: String,
    subscription_limit_reached: bool,
}

#[derive(Serialize)]
pub struct CollectionResponse {
    bookmarked: Option<CollectionItem>,
    csrfmiddlewaretoken: String,
    subscription_limit_reached: bool,
}

#[derive(Deserialize, Debug)]
pub struct CollectionCreationForm {
    pub name: String,
    pub notes: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct CollectionCreationParams {
    pub url: String,
}

#[derive(Deserialize, Debug)]
pub struct CollectionDeletionForm {
    pub delete: String,
}

#[derive(Deserialize, Debug)]
pub struct CollectionDeletionParams {
    pub url: String,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum CollectionCreationOrDeletionForm {
    Deletion(CollectionDeletionForm),
    Creation(CollectionCreationForm),
}

impl From<CollectionAndDocumentQuery> for CollectionItem {
    fn from(collection_and_document: CollectionAndDocumentQuery) -> Self {
        let mut parents: Option<Vec<CollectionParent>> = None;
        let mut title: Option<String> = None;
        let mut url = collection_and_document.uri;
        match collection_and_document.metadata {
            Some(metadata) => {
                parents = serde_json::from_value(metadata["parents"].clone()).unwrap_or(None);
                title = Some(
                    collection_and_document
                        .custom_name
                        .unwrap_or(collection_and_document.title),
                );
                url = serde_json::from_value(metadata["mdn_url"].clone()).unwrap_or(url);
            }
            None => (),
        }
        CollectionItem {
            parents: parents.unwrap_or_default(),
            created: collection_and_document.created_at,
            notes: collection_and_document.notes,
            url,
            title: title.unwrap_or_default(),
            id: collection_and_document.id,
        }
    }
}

pub async fn collections(
    _req: HttpRequest,
    id: Identity,
    pool: web::Data<Pool>,
    query: web::Query<CollectionsQueryParams>,
) -> Result<HttpResponse, ApiError> {
    match id.identity() {
        Some(id) => {
            let mut conn_pool = pool.get()?;
            let user: UserQuery = get_user(&mut conn_pool, id).await?;
            match &query.url {
                Some(url) => get_single_collection_item(pool, user, url).await,
                None => get_paginated_collection_items(pool, user, &query).await,
            }
        }
        None => Ok(HttpResponse::Unauthorized().finish()),
    }
}

async fn get_single_collection_item(
    pool: web::Data<Pool>,
    user: UserQuery,
    url: &str,
) -> Result<HttpResponse, ApiError> {
    let mut conn = pool.get()?;
    let collection = get_collection_item(user, &mut conn, url).await;
    let bookmarked = match collection {
        Ok(val) => Some(val.into()),
        Err(e) => match e {
            DbError::DieselResult(_) => None,
            _ => return Err(ApiError::Unknown),
        },
    };
    let result = CollectionResponse {
        bookmarked,
        csrfmiddlewaretoken: "abc".to_string(),
        subscription_limit_reached: false,
    };
    Ok(HttpResponse::Ok().json(result))
}

async fn get_paginated_collection_items(
    pool: Data<Pool>,
    user: UserQuery,
    query: &CollectionsQueryParams,
) -> Result<HttpResponse, ApiError> {
    let mut conn = pool.get()?;
    let collection = get_collection_items_paginated(user, &mut conn, query).await;

    let items = match collection {
        Ok(val) => val
            .iter()
            .map(|query_result| Into::<CollectionItem>::into(query_result.clone()))
            .collect(),
        Err(e) => return Err(e.into()),
    };

    //##TODO Handle subscription limits

    let result = CollectionsResponse {
        items,
        csrfmiddlewaretoken: "abc".to_string(),
        subscription_limit_reached: false,
    };
    Ok(HttpResponse::Ok().json(result))
}

pub async fn create_or_update_collection_item(
    pool: Data<Pool>,
    http_client: Data<Client>,
    id: Identity,
    query: web::Query<CollectionCreationParams>,
    collection_form: web::Form<CollectionCreationOrDeletionForm>,
) -> Result<HttpResponse, ApiError> {
    match collection_form.into_inner() {
        CollectionCreationOrDeletionForm::Creation(collection_form) => match id.identity() {
            Some(id) => {
                let mut conn_pool = pool.get()?;
                let user: UserQuery = get_user(&mut conn_pool, id).await?;
                let metadata = get_document_metadata(http_client, &query.url).await?;
                create_collection_item(
                    user,
                    &mut conn_pool,
                    query.url.clone(),
                    metadata,
                    collection_form,
                )
                .await
                .map_err(DbError::from)?;

                Ok(HttpResponse::Created().finish())
            }
            None => Ok(HttpResponse::Unauthorized().finish()),
        },
        CollectionCreationOrDeletionForm::Deletion(collection_form)
            if collection_form.delete.to_lowercase() == "true" =>
        {
            delete_collection_item(
                pool,
                id,
                web::Query(CollectionDeletionParams {
                    url: query.into_inner().url,
                }),
            )
            .await
        }
        CollectionCreationOrDeletionForm::Deletion(_) => Ok(HttpResponse::BadRequest().finish()),
    }
}

pub async fn delete_collection_item(
    pool: Data<Pool>,
    id: Identity,
    query: web::Query<CollectionDeletionParams>,
) -> Result<HttpResponse, ApiError> {
    match id.identity() {
        Some(id) => {
            let mut conn_pool = pool.get()?;
            let user: UserQuery = get_user(&mut conn_pool, id).await?;
            crate::db::collections::delete_collection_item(user, &mut conn_pool, query.url.clone())
                .await
                .map_err(DbError::from)?;
            Ok(HttpResponse::Ok().finish())
        }
        None => Ok(HttpResponse::Unauthorized().finish()),
    }
}