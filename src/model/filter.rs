use std::collections::{BTreeMap, BTreeSet};

use sea_orm::{sea_query::Expr, Condition};

use crate::control_proto as pb;

use super::entities::{item, library};

pub fn build_grouped_condition<'a, F, GFn, CFn>(
    filters: impl IntoIterator<Item = &'a F>,
    group_of: GFn,
    to_condition: CFn,
    logics: &[pb::FilterLogic],
) -> Option<Condition>
where
    F: 'a,
    GFn: Fn(&F) -> i32,
    CFn: Fn(&F) -> Option<Condition>,
{
    let mut buckets: BTreeMap<i32, Vec<Condition>> = BTreeMap::new();
    for filter in filters {
        if let Some(condition) = to_condition(filter) {
            buckets.entry(group_of(filter)).or_default().push(condition);
        }
    }
    if buckets.is_empty() {
        return None;
    }

    let group_cond = |group: i32| -> Option<Condition> {
        buckets.get(&group).map(|conditions| {
            let mut out = Condition::all();
            for condition in conditions {
                out = out.add(condition.clone());
            }
            out
        })
    };

    let mut pair_conds = Vec::new();
    let mut referenced = BTreeSet::<i32>::new();
    for logic in logics {
        let a = group_cond(logic.group_a);
        let b = group_cond(logic.group_b);
        let (a, b) = match (a, b) {
            (Some(a), Some(b)) => (a, b),
            _ => continue,
        };
        let combined = match pb::LogicOp::try_from(logic.op).unwrap_or(pb::LogicOp::And) {
            pb::LogicOp::Or => Condition::any().add(a).add(b),
            _ => Condition::all().add(a).add(b),
        };
        pair_conds.push(combined);
        referenced.insert(logic.group_a);
        referenced.insert(logic.group_b);
    }

    let mut outer = Condition::all();
    for pair in pair_conds {
        outer = outer.add(pair);
    }
    for group in buckets.keys() {
        if !referenced.contains(group) {
            if let Some(condition) = group_cond(*group) {
                outer = outer.add(condition);
            }
        }
    }
    Some(outer)
}

pub fn wallpaper_filters_to_condition(
    filters: &[pb::WallpaperFilterRule],
    logics: &[pb::FilterLogic],
) -> Option<Condition> {
    build_grouped_condition(
        filters,
        |filter| filter.group,
        wallpaper_filter_to_condition,
        logics,
    )
}

pub fn wallpaper_filter_to_condition(filter: &pb::WallpaperFilterRule) -> Option<Condition> {
    use pb::wallpaper_filter_rule::Payload;

    match pb::WallpaperFilterType::try_from(filter.r#type).ok()? {
        pb::WallpaperFilterType::Name => match filter.payload.as_ref() {
            Some(Payload::StringFilter(f)) => string_condition_to_condition(
                || Expr::col((item::Entity, item::Column::DisplayName)),
                pb::StringCondition::try_from(f.condition)
                    .unwrap_or(pb::StringCondition::Unspecified),
                &f.value,
                false,
            ),
            _ => None,
        },
        pb::WallpaperFilterType::WpType => match filter.payload.as_ref() {
            Some(Payload::StringFilter(f)) => string_condition_to_condition(
                || Expr::col((item::Entity, item::Column::Ty)),
                pb::StringCondition::try_from(f.condition)
                    .unwrap_or(pb::StringCondition::Unspecified),
                &f.value.to_ascii_lowercase(),
                false,
            ),
            _ => None,
        },
        pb::WallpaperFilterType::Library => match filter.payload.as_ref() {
            Some(Payload::StringFilter(f)) => string_condition_to_condition(
                || Expr::col((library::Entity, library::Column::Path)),
                pb::StringCondition::try_from(f.condition)
                    .unwrap_or(pb::StringCondition::Unspecified),
                &f.value,
                false,
            ),
            _ => None,
        },
        pb::WallpaperFilterType::Format => match filter.payload.as_ref() {
            Some(Payload::StringFilter(f)) => string_condition_to_condition(
                || Expr::col((item::Entity, item::Column::Format)),
                pb::StringCondition::try_from(f.condition)
                    .unwrap_or(pb::StringCondition::Unspecified),
                &f.value,
                true,
            ),
            _ => None,
        },
        pb::WallpaperFilterType::Width => match filter.payload.as_ref() {
            Some(Payload::IntFilter(f)) => int_condition_to_condition(
                || Expr::col((item::Entity, item::Column::Width)),
                pb::IntCondition::try_from(f.condition).unwrap_or(pb::IntCondition::Unspecified),
                f.value,
            ),
            _ => None,
        },
        pb::WallpaperFilterType::Height => match filter.payload.as_ref() {
            Some(Payload::IntFilter(f)) => int_condition_to_condition(
                || Expr::col((item::Entity, item::Column::Height)),
                pb::IntCondition::try_from(f.condition).unwrap_or(pb::IntCondition::Unspecified),
                f.value,
            ),
            _ => None,
        },
        pb::WallpaperFilterType::Size => match filter.payload.as_ref() {
            Some(Payload::IntFilter(f)) => int_condition_to_condition(
                || Expr::col((item::Entity, item::Column::Size)),
                pb::IntCondition::try_from(f.condition).unwrap_or(pb::IntCondition::Unspecified),
                f.value,
            ),
            _ => None,
        },
        pb::WallpaperFilterType::Aspect => match filter.payload.as_ref() {
            Some(Payload::AspectFilter(f)) => aspect_condition_to_condition(
                pb::WallpaperAspect::try_from(f.value).unwrap_or(pb::WallpaperAspect::Unspecified),
                pb::TypeCondition::try_from(f.condition).unwrap_or(pb::TypeCondition::Unspecified),
            ),
            _ => None,
        },
        pb::WallpaperFilterType::Unspecified => None,
    }
}

fn string_condition_to_condition<E>(
    col: E,
    cond: pb::StringCondition,
    value: &str,
    null_matches_negative: bool,
) -> Option<Condition>
where
    E: Fn() -> sea_orm::sea_query::Expr,
{
    match cond {
        pb::StringCondition::Contains => {
            Some(Condition::all().add(col().like(format!("%{value}%"))))
        }
        pb::StringCondition::ContainsNot => {
            let not_like = col().not_like(format!("%{value}%"));
            if null_matches_negative {
                Some(Condition::any().add(col().is_null()).add(not_like))
            } else {
                Some(Condition::all().add(not_like))
            }
        }
        pb::StringCondition::Is => Some(Condition::all().add(col().eq(value))),
        pb::StringCondition::IsNot => {
            let ne = col().ne(value);
            if null_matches_negative {
                Some(Condition::any().add(col().is_null()).add(ne))
            } else {
                Some(Condition::all().add(ne))
            }
        }
        pb::StringCondition::Unspecified => None,
    }
}

fn int_condition_to_condition<E>(col: E, cond: pb::IntCondition, value: i64) -> Option<Condition>
where
    E: Fn() -> sea_orm::sea_query::Expr,
{
    let expr = match cond {
        pb::IntCondition::Equal => col().eq(value),
        pb::IntCondition::EqualNot => col().ne(value),
        pb::IntCondition::Less => col().lt(value),
        pb::IntCondition::LessEqual => col().lte(value),
        pb::IntCondition::Greater => col().gt(value),
        pb::IntCondition::GreaterEqual => col().gte(value),
        pb::IntCondition::Unspecified => return None,
    };
    Some(Condition::all().add(expr))
}

fn aspect_condition_to_condition(
    aspect: pb::WallpaperAspect,
    cond: pb::TypeCondition,
) -> Option<Condition> {
    let width = || Expr::col((item::Entity, item::Column::Width));
    let height = || Expr::col((item::Entity, item::Column::Height));

    match cond {
        pb::TypeCondition::Is => match aspect {
            pb::WallpaperAspect::Landscape => Some(
                Condition::all()
                    .add(width().is_not_null())
                    .add(height().is_not_null())
                    .add(width().gt(height())),
            ),
            pb::WallpaperAspect::Portrait => Some(
                Condition::all()
                    .add(width().is_not_null())
                    .add(height().is_not_null())
                    .add(width().lt(height())),
            ),
            pb::WallpaperAspect::Square => Some(
                Condition::all()
                    .add(width().is_not_null())
                    .add(height().is_not_null())
                    .add(width().eq(height())),
            ),
            pb::WallpaperAspect::Unspecified => None,
        },
        pb::TypeCondition::IsNot => match aspect {
            pb::WallpaperAspect::Landscape => Some(
                Condition::any()
                    .add(width().is_null())
                    .add(height().is_null())
                    .add(width().lte(height())),
            ),
            pb::WallpaperAspect::Portrait => Some(
                Condition::any()
                    .add(width().is_null())
                    .add(height().is_null())
                    .add(width().gte(height())),
            ),
            pb::WallpaperAspect::Square => Some(
                Condition::any()
                    .add(width().is_null())
                    .add(height().is_null())
                    .add(width().ne(height())),
            ),
            pb::WallpaperAspect::Unspecified => None,
        },
        pb::TypeCondition::Unspecified => None,
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{EntityTrait, QueryFilter};

    use super::*;
    use crate::model::{
        connect_url,
        entities::item,
        repo::{self, ItemUpsertArgs},
    };

    async fn seed() -> sea_orm::DatabaseConnection {
        let db = connect_url("sqlite::memory:").await.unwrap();
        let plugin = repo::upsert_plugin(&db, "p", "1").await.unwrap();
        let lib_a = repo::add_library(&db, plugin.id, "/lib/a").await.unwrap();
        let lib_b = repo::add_library(&db, plugin.id, "/lib/b").await.unwrap();

        repo::upsert_item(
            &db,
            ItemUpsertArgs {
                plugin_id: plugin.id,
                library_id: lib_a.id,
                path: "city.png",
                ty: "image",
                display_name: "City",
                preview_path: None,
                description: None,
                external_id: None,
                size: Some(2048),
                width: Some(1920),
                height: Some(1080),
                format: Some("png"),
            },
        )
        .await
        .unwrap();
        repo::upsert_item(
            &db,
            ItemUpsertArgs {
                plugin_id: plugin.id,
                library_id: lib_b.id,
                path: "portrait.webm",
                ty: "video",
                display_name: "Portrait",
                preview_path: None,
                description: None,
                external_id: None,
                size: Some(4096),
                width: Some(900),
                height: Some(1600),
                format: Some("webm"),
            },
        )
        .await
        .unwrap();
        db
    }

    #[tokio::test]
    async fn wallpaper_filters_to_condition_matches_grouped_rules() {
        let db = seed().await;

        let mut name = pb::WallpaperFilterRule {
            r#type: pb::WallpaperFilterType::Name as i32,
            group: 0,
            payload: None,
        };
        name.payload = Some(pb::wallpaper_filter_rule::Payload::StringFilter(
            pb::WallpaperStringFilter {
                value: "City".into(),
                condition: pb::StringCondition::Contains as i32,
            },
        ));

        let mut width = pb::WallpaperFilterRule {
            r#type: pb::WallpaperFilterType::Width as i32,
            group: 0,
            payload: None,
        };
        width.payload = Some(pb::wallpaper_filter_rule::Payload::IntFilter(
            pb::WallpaperIntFilter {
                value: 1000,
                condition: pb::IntCondition::GreaterEqual as i32,
            },
        ));

        let condition = wallpaper_filters_to_condition(&[name, width], &[]).unwrap();
        let rows = item::Entity::find()
            .find_also_related(library::Entity)
            .filter(condition)
            .all(&db)
            .await
            .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.display_name, "City");
    }

    #[tokio::test]
    async fn wallpaper_filters_to_condition_honors_group_or_logic() {
        let db = seed().await;

        let mut ty = pb::WallpaperFilterRule {
            r#type: pb::WallpaperFilterType::WpType as i32,
            group: 0,
            payload: None,
        };
        ty.payload = Some(pb::wallpaper_filter_rule::Payload::StringFilter(
            pb::WallpaperStringFilter {
                value: "video".into(),
                condition: pb::StringCondition::Is as i32,
            },
        ));

        let mut aspect = pb::WallpaperFilterRule {
            r#type: pb::WallpaperFilterType::Aspect as i32,
            group: 1,
            payload: None,
        };
        aspect.payload = Some(pb::wallpaper_filter_rule::Payload::AspectFilter(
            pb::WallpaperAspectFilter {
                value: pb::WallpaperAspect::Landscape as i32,
                condition: pb::TypeCondition::Is as i32,
            },
        ));

        let logic = pb::FilterLogic {
            op: pb::LogicOp::Or as i32,
            group_a: 0,
            group_b: 1,
        };

        let condition = wallpaper_filters_to_condition(&[ty, aspect], &[logic]).unwrap();
        let rows = item::Entity::find()
            .find_also_related(library::Entity)
            .filter(condition)
            .all(&db)
            .await
            .unwrap();

        assert_eq!(rows.len(), 2);
    }
}
