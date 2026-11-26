//! The saved preview recipe: the agent reads it, `design_preview` saves it
//! once the stack answers, and the operator sees and clears it in settings.

use crate::common::design::{Fixture, PREVIEW};
use archon_core::design::recipe::{PreviewRecipe, RecipeStep, StepRun};
use archon_core::error::ArchonError;
use archon_core::protocol::{Reply, RepoRef, Request};

fn stack() -> PreviewRecipe {
    let step = |name: &str, command: &str, port: u16| RecipeStep {
        name: name.into(),
        command: command.into(),
        dir: None,
        run: StepRun::Service,
        port: Some(port),
        ready: Some(format!("http://127.0.0.1:{port}/")),
    };
    PreviewRecipe {
        source: "docs/dev-preview.md".into(),
        steps: vec![
            step("backend", "scripts/dev-preview-stack", 8000),
            step("web", "scripts/dev-preview-serve", 5173),
        ],
        env: vec!["COMPOSE_PROJECT_NAME".into()],
        notes: None,
    }
}

async fn agent_reads(f: &Fixture) -> String {
    match f
        .as_agent(Request::DesignRecipe {
            caller: Some(f.agent.clone()),
        })
        .await
        .expect("the agent reads the recipe")
    {
        Reply::DesignAgent { message } => message,
        other => panic!("design_recipe replied {other:?}"),
    }
}

async fn preview_with(f: &Fixture, recipe: PreviewRecipe) -> Result<Reply, ArchonError> {
    f.as_agent(Request::DesignPreview {
        url: PREVIEW.into(),
        recipe: Some(Box::new(recipe)),
        caller: Some(f.agent.clone()),
    })
    .await
}

async fn settings(f: &Fixture, request: Request) -> archon_core::settings::SettingsView {
    match f.operator(request).await.expect("settings answer") {
        Reply::Settings(view) => *view,
        other => panic!("settings replied {other:?}"),
    }
}

fn repo() -> RepoRef {
    RepoRef(crate::common::REPO_NAME.into())
}

#[tokio::test]
async fn a_recipe_saved_by_one_session_is_read_shown_and_cleared() {
    let f = Fixture::new().await;
    assert!(agent_reads(&f).await.contains("No preview recipe is saved"));
    preview_with(&f, stack()).await.expect("the stack answers");
    let checked = f.factory.checked_recipes.lock().unwrap().clone();
    assert_eq!(
        checked.last(),
        Some(&Some(stack())),
        "the recipe's checks ran"
    );
    let text = agent_reads(&f).await;
    assert!(
        text.contains("1. backend (service): scripts/dev-preview-stack"),
        "{text}"
    );
    assert!(
        text.contains("\"source\":\"docs/dev-preview.md\""),
        "{text}"
    );

    let view = settings(&f, Request::Settings { repo: Some(repo()) }).await;
    let saved = view
        .design_recipe
        .clone()
        .expect("settings show the recipe");
    assert_eq!(saved.recipe, stack());
    assert_eq!(saved.item, f.item);
    assert!(view.describe().contains("design preview recipe: saved"));
    let global = settings(&f, Request::Settings { repo: None }).await;
    assert!(global.design_recipe.is_none(), "recipes are per repository");

    let cleared = settings(&f, Request::ClearDesignRecipe { repo: repo() }).await;
    assert!(cleared.design_recipe.is_none());
    assert!(
        cleared
            .describe()
            .contains("design preview recipe: none saved")
    );
    assert!(agent_reads(&f).await.contains("No preview recipe is saved"));
}

#[tokio::test]
async fn a_half_started_stack_is_refused_and_saves_nothing() {
    let f = Fixture::new().await;
    *f.factory.stack_down.lock().unwrap() =
        Some("step backend: nothing listens on loopback port 8000".into());
    let refused = preview_with(&f, stack()).await.unwrap_err();
    assert!(refused.to_string().contains("step backend"), "{refused}");
    let view = settings(&f, Request::Settings { repo: Some(repo()) }).await;
    assert!(
        view.design_recipe.is_none(),
        "nothing saved for a failed stack"
    );
}

#[tokio::test]
async fn a_recipe_with_a_secret_value_is_refused_before_anything_runs() {
    let f = Fixture::new().await;
    let checks_before = f.factory.checked_recipes.lock().unwrap().len();
    let mut leaky = stack();
    leaky.steps[0].command = "DB_PASSWORD=hunter2hunter2 scripts/dev-preview-stack".into();
    let refused = preview_with(&f, leaky).await.unwrap_err();
    assert!(matches!(refused, ArchonError::Invalid(_)), "{refused:?}");
    assert!(!refused.to_string().contains("hunter2"));
    assert_eq!(
        f.factory.checked_recipes.lock().unwrap().len(),
        checks_before
    );
    let view = settings(&f, Request::Settings { repo: Some(repo()) }).await;
    assert!(view.design_recipe.is_none());
}

#[tokio::test]
async fn only_the_operator_clears_a_recipe_and_only_the_agent_reads_it() {
    let f = Fixture::new().await;
    let by_agent = f
        .as_agent(Request::ClearDesignRecipe { repo: repo() })
        .await;
    assert!(by_agent.is_err(), "an agent cannot clear it: {by_agent:?}");
    let stranger = f
        .as_agent(Request::DesignRecipe {
            caller: Some(archon_core::aether_ids::AetherAgentId::parse("agt_stranger").unwrap()),
        })
        .await;
    assert!(stranger.is_err(), "only the session's agent reads it");
}
