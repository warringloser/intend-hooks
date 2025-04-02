use actix_web::{web, post, App, HttpResponse, HttpServer, get, Responder};
use serde::{Serialize, Deserialize};
use firestore::*;
use chrono::Utc;
use env_logger;         // to initialize the logger
use futures::stream::{BoxStream, TryStreamExt};
use actix_cors::Cors;  // Add this import
use std::env;  // Add this for environment variables
use dotenv::dotenv;


async fn index() -> impl Responder {
    HttpResponse::Ok().body("Hello, Actix-web!")
}

#[derive(Deserialize, Debug)]
pub struct TaskData {
    pub text: String,
    pub _id: String,
}

#[derive(Deserialize, Debug)]
pub struct Colors {
    pub color: String,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "eventKey")]
pub enum Event {
    #[serde(rename = "nexa.change")]
    TaskChange {
        #[serde(rename="goalName")]
        goal_name: String,
        username: String,
        // This nested object holds details about the task.
        nexa: TaskData,
        colors: Colors,
    },
    #[serde(rename = "timer.pomo.workcomplete")]
    TimerEnd {
        username: String,
    },
    #[serde(other)]
    Other, // This variant will catch events you don't plan to process.
}

#[derive(Serialize, Deserialize, Debug)]
pub struct FirestoreTask {
    pub goal_name: String,
    pub username: String,
    pub task_name: String,
    pub color: String,
    pub updated_at: firestore::FirestoreTimestamp,
    pub message: Option<String>,
    pub speed_rating: Option<i32>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct User {
    pub id: String,
    pub current_task_id: String,
    pub pomodoro_spent: i32, 
}

#[derive(Serialize)]
pub struct UpdateResponse {
    pub task: Option<FirestoreTask>,
    pub user: Option<User>,
}

#[get("/healthz")]
async fn healthz() -> impl Responder {
    HttpResponse::Ok().body("OK")
}

/// Business logic for handling a TaskChange event.
pub async fn handle_task_change(
    client: &FirestoreDb,
    goal_name: String,
    username: String,
    nexa: TaskData,
    colors: Colors,
) -> Result<(FirestoreTask, User), actix_web::Error> {
    let task_name = nexa.text;
    let color = colors.color;
    
    let new_task = FirestoreTask {
        goal_name,
        username: username.clone(),
        task_name: task_name.clone(),
        color,
        updated_at: firestore::FirestoreTimestamp(Utc::now()),
        message: None,
        speed_rating: None,
    };

    // Update the task in Firestore.
    let task_updated = client.fluent()
        .update()
        .fields(paths!(FirestoreTask::{goal_name, username, task_name, color, updated_at}))
        .in_col("tasks")
        // Note: using task_name as document_id here per your changes.
        .document_id(&task_name)
        .object(&new_task)
        .execute::<FirestoreTask>()
        .await
        .map_err(|e| {
            log::error!("Failed to update Firestore task document: {}", e);
            actix_web::error::ErrorInternalServerError(
                format!("Failed to update Firestore task document: {}", e)
            )
        })?;

    // Update the user in Firestore.
    let user_updated = client.fluent()
        .update()
        .fields(paths!(User::{id, current_task_id, pomodoro_spent}))
        .in_col("users")
        .document_id(&username)
        .object(&User {
            id: username.clone(),
            current_task_id: task_name.clone(),
            pomodoro_spent: 0,
        })
        .execute::<User>()
        .await
        .map_err(|e| {
            log::error!("Failed to update Firestore user document: {}", e);
            actix_web::error::ErrorInternalServerError(
                format!("Failed to update Firestore user document: {}", e)
            )
        })?;

    Ok((task_updated, user_updated))
}

/// Business logic for handling a TimerEnd event.
pub async fn handle_timer_end(
    client: &FirestoreDb,
    username: String,
) -> Result<User, actix_web::Error> {
    let user: Option<User> = client.fluent()
        .select()
        .by_id_in("users")
        .obj()
        .one(&username)
        .await
        .map_err(|e| {
            log::error!("Failed to get Firestore user document: {}", e);
            actix_web::error::ErrorInternalServerError(
                format!("Failed to get Firestore user document: {}", e)
            )
        })?;

    let new_pomodoro_spent = match user {
        Some(user) => user.pomodoro_spent + 1,
        None => 0,
    };

    let user_updated = client.fluent()
        .update()
        .fields(paths!(User::{id, pomodoro_spent}))
        .in_col("users")
        .document_id(&username)
        .object(&User {
            id: username.clone(),
            current_task_id: "".to_string(),
            pomodoro_spent: new_pomodoro_spent,
        })
        .execute::<User>()
        .await
        .map_err(|e| {
            log::error!("Failed to update Firestore user document: {}", e);
            actix_web::error::ErrorInternalServerError(
                format!("Failed to update Firestore user document: {}", e)
            )
        })?;

    Ok(user_updated)
}

/// Consolidated business logic to process any event.
pub async fn process_event(
    client: &FirestoreDb,
    event: Event,
) -> Result<UpdateResponse, actix_web::Error> {
    let mut response = UpdateResponse {
        task: None,
        user: None,
    };

    match event {
        Event::TaskChange { goal_name, username, nexa, colors } => {
            let (task, user) = handle_task_change(client, goal_name, username, nexa, colors).await?;
            response.task = Some(task);
            response.user = Some(user);
        }
        Event::TimerEnd { username } => {
            let user = handle_timer_end(client, username).await?;
            response.user = Some(user);
        }
        _ => {}
    }
    Ok(response)
}

async fn get_user_tasks(
    client: &FirestoreDb,
    user_id: String,
) -> Result<Vec<FirestoreTask>, actix_web::Error> {
    let object_stream: BoxStream<FirestoreResult<FirestoreTask>> = client.fluent()
        .select()
        .fields(paths!(FirestoreTask::{goal_name, username, task_name, color, updated_at}))
        .from("tasks")
        .filter( |q| {
            q.field(path!(FirestoreTask::username)).eq(user_id.clone())
        })
        .order_by([(
            path!(FirestoreTask::updated_at), FirestoreQueryDirection::Descending
        )])
        .obj()
        .stream_query_with_errors()
        .await
        .map_err(|e| {
            log::error!("Failed to get Firestore task documents: {}", e);
            actix_web::error::ErrorInternalServerError(
                format!("Failed to get Firestore task documents: {}", e)
            )
        })?;


    let tasks = object_stream.try_collect::<Vec<_>>().await
        .map_err(|e| {
            log::error!("Failed to collect Firestore task documents: {}", e);
            actix_web::error::ErrorInternalServerError(
                format!("Failed to collect Firestore task documents: {}", e)
            )
        })?;
    Ok(tasks)
}

async fn get_user_current_task(
    client: &FirestoreDb,
    user_id: String,
) -> Result<Option<FirestoreTask>, actix_web::Error> {
    // First get the user to find current_task_id
    let user: Option<User> = client.fluent()
        .select()
        .by_id_in("users")
        .obj()
        .one(&user_id)
        .await
        .map_err(|e| {
            log::error!("Failed to get user: {}", e);
            actix_web::error::ErrorInternalServerError(e.to_string())
        })?;
    
    // If no user or no current task, return None
    let Some(user) = user else {
        return Ok(None);
    };
    if user.current_task_id.is_empty() {
        return Ok(None);
    }

    // Get the current task
    let task = client.fluent()
        .select()
        .by_id_in("tasks")
        .obj()
        .one(&user.current_task_id)
        .await
        .map_err(|e| {
            log::error!("Failed to get task: {}", e);
            actix_web::error::ErrorInternalServerError(e.to_string())
        })?;

    Ok(task)
}

async fn update_speed_rating(
    client: &FirestoreDb,
    task_name: String,
    speed_rating: i32,
) -> Result<FirestoreTask, actix_web::Error> {

    let find_task: Option<FirestoreTask> = client.fluent()
        .select()
        .by_id_in("tasks")
        .obj()
        .one(&task_name)
        .await
        .map_err(|e| {
            log::error!("Failed to get task: {}", e);
            actix_web::error::ErrorInternalServerError(e.to_string())
        })?;

    if find_task.is_none() {
        return Err(actix_web::error::ErrorNotFound("Task not found"));
    }

    let task = FirestoreTask {
        goal_name: "".to_string(),
        username: "".to_string(),
        task_name: task_name.clone(),
        color: "".to_string(),
        updated_at: firestore::FirestoreTimestamp(Utc::now()),
        message: None,
        speed_rating: Some(speed_rating),
    };

    let task_updated = client.fluent()
        .update()
        .fields(paths!(FirestoreTask::{updated_at, speed_rating}))
        .in_col("tasks")
        .document_id(&task_name)
        .object(&task)
        .execute::<FirestoreTask>()
        .await
        .map_err(|e| {
            log::error!("Failed to update Firestore task document: {}", e);
            actix_web::error::ErrorInternalServerError(
                format!("Failed to update Firestore task document: {}", e)
            )
        })?;

    Ok(task_updated)
}

async fn update_message(
    client: &FirestoreDb,
    user_id: String,
    task_name: String,
    message: String,
) -> Result<FirestoreTask, actix_web::Error> {
    // First check if the task exists
    let find_task: Option<FirestoreTask> = client.fluent()
        .select()
        .by_id_in("tasks")
        .obj()
        .one(&task_name)
        .await
        .map_err(|e| {
            log::error!("Failed to get task: {}", e);
            actix_web::error::ErrorInternalServerError(e.to_string())
        })?;

    if find_task.is_none() {
        return Err(actix_web::error::ErrorNotFound("Task not found"));
    }

    let new_task: FirestoreTask = FirestoreTask {
        goal_name: "".to_string(),
        username: user_id,
        task_name: task_name.clone(),
        color: "".to_string(),
        updated_at: firestore::FirestoreTimestamp(Utc::now()),
        message: Some(message),
        speed_rating: None,
    };

    let task_updated = client.fluent()
        .update()
        .fields(paths!(FirestoreTask::{updated_at, message}))
        .in_col("tasks")
        .document_id(&task_name)
        .object(&new_task)
        .execute::<FirestoreTask>()
        .await
        .map_err(|e| {
            log::error!("Failed to update Firestore task document: {}", e);
            actix_web::error::ErrorInternalServerError(
                format!("Failed to update Firestore task document: {}", e)
            )
        })?;

    Ok(task_updated)
}

/// HTTP endpoint that wraps the business logic.
#[post("/webhook")]
async fn process_update(
    client: web::Data<FirestoreDb>,
    payload: web::Json<Event>,
) -> Result<HttpResponse, actix_web::Error> {
    let event = payload.into_inner();
    let response = process_event(&client, event).await?;
    Ok(HttpResponse::Ok().json(response))
}

#[post("/users/{userId}/tasks/{taskName}/speedRating")]
async fn update_speed_rating_handler(
    client: web::Data<FirestoreDb>,
    path: web::Path<(String, String)>,
    payload: web::Json<i32>,
) -> Result<HttpResponse, actix_web::Error> {
    let (_user_id, task_name) = path.into_inner();
    let speed_rating = payload.into_inner();
    let response = update_speed_rating(&client, task_name, speed_rating).await?;
    Ok(HttpResponse::Ok().json(response))
}

#[post("/users/{userId}/tasks/{taskName}/message")]
async fn update_message_handler(
    client: web::Data<FirestoreDb>,
    path: web::Path<(String, String)>,
    payload: web::Json<String>,
) -> Result<HttpResponse, actix_web::Error> {
    let (user_id, task_name) = path.into_inner();
    let message = payload.into_inner();
    let response = update_message(&client, user_id, task_name, message).await?;
    Ok(HttpResponse::Ok().json(response))
}

#[get("/users/{userId}/tasks")]
async fn get_user_tasks_handler(
    client: web::Data<FirestoreDb>,
    path: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    let user_id = path.into_inner();
    let tasks = get_user_tasks(&client, user_id).await?;
    Ok(HttpResponse::Ok().json(tasks))
}

#[get("/users/{userId}/currentTask")]
async fn get_user_current_task_handler(
    client: web::Data<FirestoreDb>,
    path: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    let user_id = path.into_inner();
    let task = get_user_current_task(&client, user_id).await?;
    Ok(HttpResponse::Ok().json(task))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv().ok(); // Load .env file
    env_logger::init();
    
    // You can set these via environment variables
    let frontend_origin = env::var("FRONTEND_ORIGIN")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());

    let firestore_client = FirestoreDb::with_options_service_account_key_file(
        FirestoreDbOptions::new("intend-web-hooks".to_string()),
        "intend-web-hooks-firebase-adminsdk-fbsvc-84bfcf7574.json".into(),
    )
    .await
    .expect("Failed to create Firestore client");

    let client_data = web::Data::new(firestore_client);

    HttpServer::new(move || {
        // Create CORS middleware
        let cors = Cors::default()
            .allowed_origin(&frontend_origin)
            // Optionally allow multiple origins
            // .allowed_origin("http://localhost:5173")
            // .allowed_origin("https://your-production-domain.com")
            .allowed_methods(vec!["GET", "POST"])
            .allowed_headers(vec![
                actix_web::http::header::AUTHORIZATION,
                actix_web::http::header::ACCEPT,
                actix_web::http::header::CONTENT_TYPE,
            ])
            .max_age(3600); // Cache CORS preflight for 1 hour

        App::new()
            .wrap(cors)  // Add the CORS middleware
            .app_data(client_data.clone())
            .route("/", web::get().to(index))
            .service(process_update)
            .service(healthz)
            .service(get_user_tasks_handler)
            .service(get_user_current_task_handler)
            .service(update_speed_rating_handler)
            .service(update_message_handler)
    })
    .bind("127.0.0.1:8000")?
    .run()
    .await
}
