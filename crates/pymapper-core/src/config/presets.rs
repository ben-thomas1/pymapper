//! Framework presets: entrypoint rules for code that frameworks call by convention
//! (design: dead code (d)). Patterns follow [`super::EntrypointConfig`] semantics: decorators are
//! matched as written, bases as fully qualified names, names without `.`/`:` against the bare
//! symbol name.

use super::name_enum;

name_enum!(
    /// A framework preset; enabling one adds its [`PresetRules`] to the entrypoint rules.
    Preset, "preset", {
        /// Flask routes, hooks, CLI commands and class-based views.
        Flask = "flask",
        /// FastAPI / Starlette routes, event handlers and dependencies.
        Fastapi = "fastapi",
        /// Django models, views, admin, apps, forms, commands, signals, URL conf names.
        Django = "django",
        /// Click commands and groups.
        Click = "click",
        /// pytest tests, fixtures, hooks and unittest-style classes.
        Pytest = "pytest",
        /// Pydantic models, validators and serialisers.
        Pydantic = "pydantic",
    }
);

/// Entrypoint patterns contributed by one preset.
#[derive(Clone, Copy, Debug)]
pub struct PresetRules {
    /// Decorator globs.
    pub decorators: &'static [&'static str],
    /// Fully qualified base-class globs.
    pub bases: &'static [&'static str],
    /// Name globs.
    pub names: &'static [&'static str],
}

const HTTP_VERBS: [&str; 7] = ["*.get", "*.post", "*.put", "*.patch", "*.delete", "*.head", "*.options"];

const FLASK: PresetRules = PresetRules {
    decorators: &[
        "*.route",
        HTTP_VERBS[0],
        HTTP_VERBS[1],
        HTTP_VERBS[2],
        HTTP_VERBS[3],
        HTTP_VERBS[4],
        "*.before_request",
        "*.before_app_request",
        "*.after_request",
        "*.after_app_request",
        "*.teardown_request",
        "*.teardown_app_request",
        "*.teardown_appcontext",
        "*.errorhandler",
        "*.app_errorhandler",
        "*.context_processor",
        "*.app_context_processor",
        "*.url_value_preprocessor",
        "*.url_defaults",
        "*.template_filter",
        "*.app_template_filter",
        "*.template_global",
        "*.app_template_global",
        "*.template_test",
        "*.app_template_test",
        "*.shell_context_processor",
        "*.cli.command",
        "*.cli.group",
    ],
    bases: &["flask.views.View", "flask.views.MethodView"],
    names: &["create_app", "make_app"],
};

const FASTAPI: PresetRules = PresetRules {
    decorators: &[
        HTTP_VERBS[0],
        HTTP_VERBS[1],
        HTTP_VERBS[2],
        HTTP_VERBS[3],
        HTTP_VERBS[4],
        HTTP_VERBS[5],
        HTTP_VERBS[6],
        "*.trace",
        "*.api_route",
        "*.route",
        "*.websocket",
        "*.websocket_route",
        "*.on_event",
        "*.middleware",
        "*.exception_handler",
    ],
    bases: &[
        "starlette.middleware.base.BaseHTTPMiddleware",
        "starlette.endpoints.HTTPEndpoint",
        "starlette.endpoints.WebSocketEndpoint",
    ],
    names: &["lifespan"],
};

const DJANGO: PresetRules = PresetRules {
    decorators: &[
        "receiver",
        "*.receiver",
        "*.register",
        "*.register.filter",
        "*.register.simple_tag",
        "*.register.inclusion_tag",
        "*.register.tag",
        "*.display",
        "*.action",
    ],
    bases: &[
        "django.db.models.Model",
        "django.db.models.Manager",
        "django.db.models.QuerySet",
        "django.db.migrations.Migration",
        "django.apps.AppConfig",
        "django.views.View",
        "django.views.generic.*",
        "django.contrib.admin.ModelAdmin",
        "django.contrib.admin.TabularInline",
        "django.contrib.admin.StackedInline",
        "django.forms.Form",
        "django.forms.ModelForm",
        "django.core.management.base.BaseCommand",
        "django.test.TestCase",
        "django.test.SimpleTestCase",
        "django.test.TransactionTestCase",
        "django.template.Library",
        "django.utils.deprecation.MiddlewareMixin",
    ],
    names: &[
        "urlpatterns",
        "app_name",
        "handler400",
        "handler403",
        "handler404",
        "handler500",
        "Meta",
        "clean_*",
        "default_app_config",
    ],
};

const CLICK: PresetRules = PresetRules {
    decorators: &[
        "click.command",
        "click.group",
        "*.command",
        "*.group",
        "*.result_callback",
        "*.resultcallback",
    ],
    bases: &["click.Command", "click.Group", "click.ParamType", "click.MultiCommand"],
    names: &[],
};

const PYTEST: PresetRules = PresetRules {
    decorators: &[
        "fixture",
        "pytest.fixture",
        "*.fixture",
        "hookimpl",
        "pytest.hookimpl",
        "*.hookimpl",
        "pytest_asyncio.fixture",
    ],
    bases: &["unittest.TestCase", "unittest.IsolatedAsyncioTestCase"],
    names: &[
        "test_*",
        "Test*",
        "pytest_*",
        "setup_module",
        "teardown_module",
        "setup_function",
        "teardown_function",
        "setup_class",
        "teardown_class",
        "setup_method",
        "teardown_method",
        "setUp",
        "tearDown",
        "setUpClass",
        "tearDownClass",
        "setUpModule",
        "tearDownModule",
        "asyncSetUp",
        "asyncTearDown",
    ],
};

const PYDANTIC: PresetRules = PresetRules {
    decorators: &[
        "validator",
        "*.validator",
        "root_validator",
        "*.root_validator",
        "field_validator",
        "*.field_validator",
        "model_validator",
        "*.model_validator",
        "field_serializer",
        "*.field_serializer",
        "model_serializer",
        "*.model_serializer",
        "computed_field",
        "*.computed_field",
    ],
    bases: &[
        "pydantic.BaseModel",
        "pydantic.main.BaseModel",
        "pydantic.RootModel",
        "pydantic_settings.BaseSettings",
        "pydantic.BaseSettings",
    ],
    names: &["model_config", "model_post_init", "Config"],
};

impl Preset {
    /// The entrypoint patterns this preset adds.
    pub fn rules(self) -> &'static PresetRules {
        match self {
            Preset::Flask => &FLASK,
            Preset::Fastapi => &FASTAPI,
            Preset::Django => &DJANGO,
            Preset::Click => &CLICK,
            Preset::Pytest => &PYTEST,
            Preset::Pydantic => &PYDANTIC,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::GlobSet;
    use super::*;

    #[test]
    fn every_preset_pattern_is_a_valid_glob() {
        for &p in Preset::ALL {
            let r = p.rules();
            for list in [r.decorators, r.bases, r.names] {
                GlobSet::new(list).unwrap_or_else(|(i, e)| panic!("{p}: {:?}: {e}", list[i]));
            }
        }
    }

    #[test]
    fn names_round_trip() {
        for &p in Preset::ALL {
            assert_eq!(p.as_str().parse::<Preset>(), Ok(p));
        }
        let err = "fastpi".parse::<Preset>().unwrap_err();
        assert_eq!(err.suggestion, Some("fastapi"));
    }
}
