use super::*;

fn step(name: &str, command: &str) -> RecipeStep {
    RecipeStep {
        name: name.into(),
        command: command.into(),
        dir: None,
        run: StepRun::Service,
        port: None,
        ready: None,
    }
}

fn recipe(steps: Vec<RecipeStep>) -> PreviewRecipe {
    PreviewRecipe {
        source: "docs/dev-preview.md".into(),
        steps,
        env: vec![],
        notes: None,
    }
}

fn refused(recipe: &PreviewRecipe) -> String {
    recipe.check().expect_err("the recipe is refused")
}

#[test]
fn a_multi_service_recipe_passes_and_round_trips() {
    let mut db = step(
        "stack",
        "docker compose -p \"$COMPOSE_PROJECT_NAME\" -f scripts/dev/compose.yml up -d",
    );
    db.port = Some(5432);
    let mut seed = step("seed", "npm run seed -- --password \"$DB_PASSWORD\"");
    seed.run = StepRun::Once;
    let mut web = step("web", "setsid npm run dev -- --host 127.0.0.1 --port 5173");
    web.dir = Some("web".into());
    web.port = Some(5173);
    web.ready = Some("http://127.0.0.1:5173/".into());
    let mut r = recipe(vec![db, seed, web]);
    r.env = vec!["DB_PASSWORD".into(), "COMPOSE_PROJECT_NAME".into()];
    r.notes = Some("open /fast-check/ after the seed".into());
    r.check().unwrap();
    let json = serde_json::to_string(&r).unwrap();
    assert_eq!(serde_json::from_str::<PreviewRecipe>(&json).unwrap(), r);
    let text = r.describe();
    assert!(text.contains("2. seed (once): npm run seed"), "{text}");
    assert!(text.contains("3. web (service)"), "{text}");
    assert!(text.contains("[in web] port 5173 ready at http://127.0.0.1:5173/"));
    assert!(text.contains("env names: DB_PASSWORD, COMPOSE_PROJECT_NAME"));
}

#[test]
fn a_single_server_recipe_needs_only_a_name_and_command() {
    let r: PreviewRecipe = serde_json::from_value(serde_json::json!({
        "source": "package.json scripts.dev",
        "steps": [{"name": "web", "command": "npm run dev -- --host 127.0.0.1"}]
    }))
    .unwrap();
    r.check().unwrap();
    assert_eq!(r.steps[0].run, StepRun::Service);
}

#[test]
fn malformed_recipes_are_refused_with_the_field() {
    assert!(refused(&recipe(vec![])).contains("1 to 12 steps"));
    let many = (0..13).map(|i| step(&format!("s{i}"), "true")).collect();
    assert!(refused(&recipe(many)).contains("1 to 12 steps"));
    assert!(refused(&recipe(vec![step("Web", "npm start")])).contains("step name"));
    let twice = recipe(vec![step("web", "a"), step("web", "b")]);
    assert!(refused(&twice).contains("used twice"));
    assert!(refused(&recipe(vec![step("web", "  ")])).contains("command is empty"));
    let mut outside = step("web", "npm start");
    outside.dir = Some("../other".into());
    assert!(refused(&recipe(vec![outside])).contains("inside the worktree"));
    let mut absolute = step("web", "npm start");
    absolute.dir = Some("/etc".into());
    assert!(refused(&recipe(vec![absolute])).contains("inside the worktree"));
    let mut remote = step("web", "npm start");
    remote.ready = Some("http://example.com/".into());
    assert!(refused(&recipe(vec![remote])).contains("ready"));
    let mut zero = step("web", "npm start");
    zero.port = Some(0);
    assert!(refused(&recipe(vec![zero])).contains("nonzero"));
    let mut blank = recipe(vec![step("web", "npm start")]);
    blank.source = "a\nb".into();
    assert!(refused(&blank).contains("one line"));
}

#[test]
fn unknown_fields_are_refused_on_parse() {
    let parsed = serde_json::from_value::<PreviewRecipe>(serde_json::json!({
        "source": "README.md",
        "steps": [{"name": "web", "command": "make dev", "env": {"TOKEN": "x"}}]
    }));
    assert!(parsed.is_err());
}

#[test]
fn env_holds_names_and_never_values() {
    let mut r = recipe(vec![step("web", "npm start")]);
    r.env = vec!["API_TOKEN=abc123".into()];
    let error = refused(&r);
    assert!(error.contains("names only"), "{error}");
    assert!(
        !error.contains("abc123"),
        "the value is never echoed: {error}"
    );
    r.env = vec!["1BAD".into()];
    assert!(refused(&r).contains("not a variable name"));
    r.env = vec!["A".into(), "A".into()];
    assert!(refused(&r).contains("twice"));
}

#[test]
fn spelled_out_secrets_are_refused_and_never_echoed() {
    for command in [
        "API_TOKEN=hunter2hunter2 npm start",
        "env DB_PASSWORD='s3cret' ./serve",
        "./serve --api-key abcdef123",
        "./serve --password=s3cret",
        "psql postgres://app:s3cret@127.0.0.1:5432/app",
        "GH=ghp_0123456789abcdefABCDEF ./serve",
        "curl -H x sk-live0123456789abcdefgh",
        "PGPASSWORD=hunter2 psql -h 127.0.0.1 app",
        "export A=1;TOKEN=hunter2 && ./serve",
        "API_TOKEN=/hunter2 ./serve",
        "DATABASE_URL=postgres://app:/hunter2@127.0.0.1:5432/app ./serve",
        "curl -u admin:hunter2 http://127.0.0.1:8000/health",
        "curl --user=admin:hunter2 http://127.0.0.1:8000/health",
        "mysql -uroot -phunter2 app",
        "curl -H \"Authorization: Bearer hunter2hunter2\" http://127.0.0.1:8000/",
        "git clone https://ghp_0123456789abcdefABCDEF@github.com/o/r",
        "TOKEN=${PREFIX}hunter2 ./serve",
        "./serve --password=$PREFIX'hunter2'",
        "./serve --password \"$PREFIX\"hunter2",
        "psql postgres://app:${P}hunter2@127.0.0.1:5432/app",
        "PASSWORD=$(echo hunter2) ./serve",
        "DATABASE_URL=postgres://app:hu/nter2@127.0.0.1:5432/app ./serve",
        "./serve \"--api-key=hunter2\"",
        "curl -uadmin:hunter2 http://127.0.0.1:8000/",
        "curl -suadmin:hunter2 http://127.0.0.1:8000/",
        "curl -su admin:hunter2 http://127.0.0.1:8000/",
        "http -a admin:hunter2 127.0.0.1:8000/",
        "https -aadmin:hunter2 127.0.0.1:8443/",
    ] {
        let error = refused(&recipe(vec![step("web", command)]));
        assert!(error.contains("secret"), "{command}: {error}");
        for value in ["hunter2", "s3cret", "abcdef123", "0123456789abcdef"] {
            assert!(!error.contains(value), "{command} echoed: {error}");
        }
    }
    let mut notes = recipe(vec![step("web", "npm start")]);
    notes.notes = Some("SECRET_KEY=abc".into());
    assert!(refused(&notes).contains("notes"));
}

#[test]
fn references_and_ordinary_words_are_not_secrets() {
    for command in [
        "API_TOKEN=$API_TOKEN npm start",
        "./serve --password \"$DB_PASSWORD\" --port 8000",
        "psql postgres://app:${DB_PASSWORD}@127.0.0.1:5432/app",
        "psql postgres://app@127.0.0.1:5432/app",
        "npm run dev -- --host 127.0.0.1 --port 5173",
        "KEYBOARD_LAYOUT=us MONKEY=1 ./serve",
        "./serve --auth-file .archon/auth.json --no-auth",
        "pip install scikit-learn sk-learn",
        "GOOGLE_APPLICATION_CREDENTIALS=$GOOGLE_APPLICATION_CREDENTIALS ./serve",
        "PGPASSWORD=$PGPASSWORD psql -h 127.0.0.1 app",
        "TOKENIZERS_PARALLELISM=false ./serve",
        "docker run -u 1000:1000 app",
        "curl -u \"$API_USER:$API_PASS\" http://127.0.0.1:8000/health",
        "curl --user=admin http://127.0.0.1:8000/health",
        "mysql -u root -p\"$DB_PASSWORD\" app",
        "curl -H \"Authorization: Bearer $API_TOKEN\" http://127.0.0.1:8000/",
        "make dev; npm run dev && ./serve",
        "curl http://127.0.0.1:5173/@vite/client",
        "curl \"http://[::1]/@vite/client\"",
        "curl http://127.0.0.1:5173/@fs/web/src/main.ts",
        "./serve --title \"the app's page\"",
        "curl -sSfu \"$API_USER:$API_PASS\" http://127.0.0.1:8000/health",
        "curl -sSf -H 'User-Agent: archon' http://127.0.0.1:8000/health",
        "http -a \"$API_USER:$API_PASS\" 127.0.0.1:8000/",
    ] {
        recipe(vec![step("web", command)])
            .check()
            .unwrap_or_else(|e| panic!("{command}: {e}"));
    }
}

#[test]
fn compose_projects_are_named_only_through_the_run_variable() {
    for command in [
        "docker compose up -d",
        "docker compose -p \"$COMPOSE_PROJECT_NAME\" up -d",
        "docker compose --project-name=${COMPOSE_PROJECT_NAME} up -d",
        "COMPOSE_PROJECT_NAME=$COMPOSE_PROJECT_NAME scripts/dev-stack",
        "tar -p -cf out.tar web",
        "docker compose -f compose.yml -p \"$COMPOSE_PROJECT_NAME\" up -d",
        "docker compose -p$COMPOSE_PROJECT_NAME up -d",
        "docker compose -p\"$COMPOSE_PROJECT_NAME\" up -d",
        "docker compose -f 'compose dev.yml' -p \"$COMPOSE_PROJECT_NAME\" up -d",
        "docker compose up -d && psql -h 127.0.0.1 -p 5432 app",
        "docker compose --dry-run up -d",
        "docker compose --profile web --env-file .env.dev up -d",
        "docker compose build web && docker compose run --rm web npm test",
        "sh -c 'docker compose -p \"$COMPOSE_PROJECT_NAME\" up -d'",
        "sh -c \"docker compose -p \\\"\\$COMPOSE_PROJECT_NAME\\\" up -d\"",
        "docker compose up -d 2>&1 | tee compose.log",
        "docker compose \\\n  -p \"$COMPOSE_PROJECT_NAME\" up -d",
        "docker compose build web \\\n  && docker compose up -d",
        "sh -c 'docker compose \\\n -p \"$COMPOSE_PROJECT_NAME\" up -d'",
        "docker compose -f 'compose dev.yml' up -d",
        "docker compose --profile \"$COMPOSE_PROFILE\" up -d",
        "docker compose --env-file \"$ENV_FILE\" up -d",
        "docker compose -f \"$COMPOSE_FILE\" --profile=\"$PROFILE\" up -d",
        "docker compose -p \"$COMPOSE_PROJECT_NAME\" --env-file \"${ENV_FILE:-.env}\" up -d",
        "sh -c 'docker compose --profile \"$COMPOSE_PROFILE\" up -d'",
    ] {
        recipe(vec![step("stack", command)])
            .check()
            .unwrap_or_else(|e| panic!("{command}: {e}"));
    }
    for command in [
        "docker compose -p ccso up -d",
        "docker-compose --project-name shop up",
        "docker compose --project-name=shop up",
        "COMPOSE_PROJECT_NAME=ccso-dev scripts/dev-stack",
        "docker compose -pother up -d",
        "docker compose -p=other up -d",
        "docker compose -f compose.yml -p other up -d",
        "docker compose -p",
        "cd web;COMPOSE_PROJECT_NAME=ccso docker compose up",
        "docker compose -f 'compose dev.yml' -p other up -d",
        "docker compose --parallel 4 -p other up -d",
        "docker compose -p\"other\" up -d",
        "docker compose -p\\ other up -d",
        "docker compose --project-name \"$COMPOSE_PROJECT_NAME\"x up -d",
        "docker compose --profile build -p other up -d",
        "docker compose --env-file up -p other up -d",
        "docker compose --a-new-option run -p other up -d",
        "docker compose --dry-run -p other up -d",
        "docker compose -fcompose.yml -p other up -d",
        "docker compose up -d -p other",
        "docker compose --project-name other up -d",
        "sh -c 'docker compose -p other up -d'",
        "docker compose -p \"$COMPOSE_PROJECT_NAME\" -p other up -d",
        "sh -c 'docker compose -\\p other up -d'",
        "sh -c 'docker comp\\ose -p other up -d'",
        "P=-p; docker compose $P other up -d",
        "docker compose $(echo -p) other up -d",
        "docker compose `echo -p` other up -d",
        "docker compose $'-p' other up -d",
        "podman-compose -p other up -d",
        "docker compose \\\n -p other up -d",
        "sh -c 'docker compose\\\n -p other up -d'",
        "sh -c \"docker compose \\\\\n-p other up -d\"",
        "sh -c 'docker compose up \";\" -p other'",
        "sh -c 'docker compose up \\; -p other'",
        "sh -c \"sh -c 'docker compose up \\\";\\\" -p other'\"",
        "sh -c 'docker compose -f \"x.yml -p other up'",
        "docker compose --profile $PROFILE up -d",
        "docker compose --profile=$PROFILE up -d",
        "docker compose up -d \"$SERVICE\"",
        "docker compose --profile --env-file \"$X\" up -d",
        "docker compose -d --profile \"$X\" up",
        "eval docker compose --profile '$X' up -d",
        "sh -c \"docker compose --profile $X up -d\"",
        "sh -c 'docker compose --profile '$X' up -d'",
        "docker compose --profile \"`echo x`\" `echo -p` other up -d",
        "x=`docker compose -p other up -d`",
    ] {
        let error = refused(&recipe(vec![step("stack", command)]));
        assert!(
            error.contains("$COMPOSE_PROJECT_NAME"),
            "{command}: {error}"
        );
    }
}

#[test]
fn a_published_port_is_refused_with_a_hint() {
    for command in [
        "docker compose run -p 8080:80 web",
        "docker compose -f compose.yml run -p 8080:80 web",
        "docker compose exec db psql -p 5432 app",
    ] {
        let error = refused(&recipe(vec![step("stack", command)]));
        assert!(
            error.contains("$COMPOSE_PROJECT_NAME")
                && error.contains("publish ports in the compose file"),
            "{command}: {error}"
        );
    }
}

#[test]
fn a_saved_recipe_says_when_and_by_whom() {
    let saved = SavedRecipe {
        recipe: recipe(vec![step("web", "npm start")]),
        saved_at: chrono::DateTime::parse_from_rfc3339("2026-11-26T10:00:00Z")
            .unwrap()
            .to_utc(),
        item: QueueItemId::new(),
    };
    let text = saved.describe();
    assert!(
        text.starts_with("saved 2026-11-26 10:00 UTC by item qi_"),
        "{text}"
    );
    assert!(text.contains("1. web (service): npm start"));
    let json = serde_json::to_value(&saved).unwrap();
    assert_eq!(serde_json::from_value::<SavedRecipe>(json).unwrap(), saved);
}

#[test]
fn a_command_with_an_unclosed_quote_is_refused() {
    let error = refused(&recipe(vec![step("web", "./serve --title 'oops")]));
    assert!(error.contains("never closed"), "{error}");
}

#[test]
fn readiness_urls_with_an_at_sign_in_the_path_are_accepted() {
    let mut web = step("web", "npm run dev");
    web.ready = Some("http://127.0.0.1:5173/@vite/client".into());
    recipe(vec![web]).check().unwrap();
}
