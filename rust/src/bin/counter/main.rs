use std::{env, time::Duration};

use chrono::prelude::*;

use axum::{
    error_handling::HandleErrorLayer,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use counter_entity::{line_items, orders, orders::Entity as Order};
use sea_orm::{
    prelude::Decimal, ActiveModelTrait, Database, DatabaseConnection, EntityTrait, ModelTrait, Set,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use tower::{BoxError, ServiceBuilder};
use tower_http::trace::{self, TraceLayer};
use tracing::Level;
use tracing_subscriber::{prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

#[derive(Debug, Parser, Clone)]
struct Config {
    #[clap(default_value = "localhost", env)]
    host: String,
    #[clap(default_value = "5002", env)]
    app_port: u16,
    #[clap(default_value = "postgres://postgres:P@ssw0rd@127.0.0.1/postgres", env)]
    database_url: String,
    #[clap(default_value = "http://localhost:5001", env)]
    product_url: String,
}

#[derive(Clone)]
struct AppState {
    config: Config,
    db_conn: DatabaseConnection,
}

#[tokio::main]
async fn main() {
    env::set_var("RUST_LOG", "debug");
    dotenv::dotenv().ok();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "counter_api=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::parse();

    let db_conn: DatabaseConnection = Database::connect(&config.database_url)
        .await
        .expect("Database connection failed");

    let state = AppState {
        config: config.clone(),
        db_conn,
    };

    let app = Router::new()
        .route("/", get(home_handler))
        .route("/v1/api/fulfillment-orders", get(get_order_handler))
        .route("/v1/api/orders", post(place_order_handler))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|error: BoxError| async move {
                    if error.is::<tower::timeout::error::Elapsed>() {
                        Ok(StatusCode::REQUEST_TIMEOUT)
                    } else {
                        Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Unhandled internal error: {}", error),
                        ))
                    }
                }))
                .timeout(Duration::from_secs(10))
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
                        .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
                )
                .into_inner(),
        )
        .with_state(state);

    let addr: String = format!("{}:{}", config.host.as_str(), config.app_port);

    tracing::debug!("listening on {}", addr);

    axum::Server::bind(&addr.parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaceOrderItem {
    item_type: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaceOrder {
    command_type: Option<i32>,
    order_source: Option<i32>,
    location: Option<i32>,
    loyalty_member_id: Option<Uuid>,
    barista_items: Option<Vec<PlaceOrderItem>>,
    kitchen_items: Option<Vec<PlaceOrderItem>>,
    timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OrderModel {
    pub id: Uuid,
    pub order_source: i32,
    pub loyalty_member_id: Uuid,
    pub order_status: i32,
    pub order_lines: Vec<OrderLineModel>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VecOrderLineModel(Vec<OrderLineModel>);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OrderLineModel {
    pub id: Uuid,
    pub item_type: i32,
    pub name: String,
    pub price: Decimal,
    pub item_status: i32,
    pub is_barista_order: bool,
    pub order_id: Option<Uuid>,
}

async fn get_order_handler(State(app): State<AppState>) -> impl IntoResponse {
    let ord = Order::find().all(&app.db_conn).await;

    match ord {
        Ok(result) => {
            let mut temp = vec![];
            for order in result {
                let mut order_model = OrderModel {
                    id: order.id,
                    loyalty_member_id: order.loyalty_member_id,
                    order_source: order.order_source,
                    order_status: order.order_status,
                    order_lines: Vec::new(),
                };

                let line_items = order
                    .find_related(line_items::Entity)
                    .all(&app.db_conn)
                    .await
                    .unwrap_or_default();
                for line_item in line_items {
                    order_model.order_lines.push(OrderLineModel {
                        id: line_item.id,
                        is_barista_order: line_item.is_barista_order,
                        item_status: line_item.item_status,
                        item_type: line_item.item_type,
                        name: line_item.name,
                        order_id: line_item.order_id,
                        price: line_item.price,
                    })
                }

                temp.push(order_model);
            }

            (StatusCode::OK, Json(temp))
        }
        Err(err) => {
            // bail!("err: {}", err)
            (StatusCode::OK, Json(vec![]))
        }
    }
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemTypeDto {
    price: f32,
    item_type: i32,
}

async fn place_order_handler(
    State(app): State<AppState>,
    Json(input): Json<PlaceOrder>,
) -> impl IntoResponse {
    let txn = app.db_conn.begin().await.unwrap();

    let result = orders::ActiveModel {
        order_source: Set(input.order_source.unwrap_or(0)),
        loyalty_member_id: Set(input.loyalty_member_id.unwrap_or_default()),
        order_status: Set(1),
        ..Default::default()
    }
    .save(&app.db_conn)
    .await
    .unwrap();

    // barista
    let barista_items_vec = input.barista_items.unwrap();
    if barista_items_vec.iter().len() > 0 {
        let params = params_process(&barista_items_vec);
        let product_items = get_product_items(&app.config.product_url, params).await;
        tracing::debug!("product_items: {:?}", product_items);

        for barista_item in barista_items_vec {
            let product_item_result = product_items
                .clone()
                .into_iter()
                .find(|i| i.item_type == barista_item.item_type.unwrap_or_default());

            let price = if let Some(product_item) = product_item_result {
                product_item.price
            } else {
                0.0
            };

            let _ = line_items::ActiveModel {
                item_type: Set(barista_item.item_type.unwrap_or_default()),
                name: Set(barista_item.item_type.unwrap_or_default().to_string()),
                price: Set(Decimal::from_f32_retain(price).unwrap_or_default()),
                item_status: Set(0),
                is_barista_order: Set(true),
                order_id: result.id.clone().into(),
                ..Default::default()
            }
            .save(&app.db_conn)
            .await
            .unwrap();
        }
    }

    // kitchen
    let kitchen_items_vec = input.kitchen_items.unwrap();
    if kitchen_items_vec.iter().len() > 0 {
        let params = params_process(&kitchen_items_vec);
        let product_items = get_product_items(&app.config.product_url, params).await;
        tracing::debug!("product_items: {:?}", product_items);

        for kitchen_item in kitchen_items_vec {
            let product_item_result = product_items
                .clone()
                .into_iter()
                .find(|i| i.item_type == kitchen_item.item_type.unwrap_or_default());

            let price = if let Some(product_item) = product_item_result {
                product_item.price
            } else {
                0.0
            };

            let _ = line_items::ActiveModel {
                item_type: Set(kitchen_item.item_type.unwrap_or_default()),
                name: Set(kitchen_item.item_type.unwrap_or_default().to_string()),
                price: Set(Decimal::from_f32_retain(price).unwrap_or_default()),
                item_status: Set(0),
                is_barista_order: Set(false),
                order_id: result.id.clone().into(),
                ..Default::default()
            }
            .save(&app.db_conn)
            .await
            .unwrap();
        }
    }

    txn.commit().await.unwrap();

    result.id.unwrap().to_string()
}

async fn home_handler() -> impl IntoResponse {
    StatusCode::OK
}

fn params_process(items_vec: &[PlaceOrderItem]) -> String {
    let params = items_vec.iter().fold("".to_string(), |acc, x| {
        if let Some(item_type) = x.item_type {
            tracing::debug!("item_type: {:?}", x);
            format!("{acc},{}", item_type)
        } else {
            "".to_string()
        }
    });

    params
}

async fn get_product_items(product_url: &str, params: String) -> Vec<ItemTypeDto> {
    let url = format!(
        "{}/v1/api/items-by-types/{}",
        product_url,
        params.trim_start_matches(',')
    );
    tracing::debug!("url: {}", url);

    reqwest::get(url)
        .await
        .unwrap()
        .json::<Vec<ItemTypeDto>>()
        .await
        .unwrap()
}