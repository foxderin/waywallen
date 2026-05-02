module;
#include "QExtra/macro_qt.hpp"

#ifdef Q_MOC_RUN
#    include "waywallen/query/settings_query.moc"
#endif

export module waywallen:query.settings;
export import :query.query;

namespace waywallen
{

/// Fetch the daemon's persisted settings. `global` is a flat
/// QVariantMap (`targetExtent`, `renderSizePolicy`, `layoutDefaults`);
/// `renderSizePolicy` is the int value of
/// `control::v1::RenderSizePolicy`. `plugins` is keyed by plugin name
/// with each value a `{key: stringValue}` QVariantMap. Plugin values
/// are wire-string typed — the QML form coerces per the matching
/// `SettingSchema.type`.
export class SettingsGetQuery : public Query, public QueryExtra<control::v1::Response, SettingsGetQuery> {
    Q_OBJECT
    QML_ELEMENT

    Q_PROPERTY(QVariantMap global READ global NOTIFY globalChanged FINAL)
    Q_PROPERTY(QVariantMap plugins READ plugins NOTIFY pluginsChanged FINAL)

public:
    SettingsGetQuery(QObject* parent = nullptr);

    auto global() const -> const QVariantMap&;
    auto plugins() const -> const QVariantMap&;

    void reload() override;

    Q_SIGNAL void globalChanged();
    Q_SIGNAL void pluginsChanged();

private:
    QVariantMap m_global;
    QVariantMap m_plugins;
};

/// Apply a full-replace settings write. Caller must populate both
/// `global` (QVariantMap with `targetExtent`/`renderSizePolicy`;
/// missing keys default to 0 / ONE_AXIS_AUTO) and `plugins`
/// (`{plugin: {key: stringValue}}`). The
/// daemon validates against the manifest schema (range, enum) and
/// returns INVALID_ARGUMENT on rejection — surfaced via the standard
/// Query `error` property.
export class SettingsSetQuery : public Query, public QueryExtra<control::v1::Response, SettingsSetQuery> {
    Q_OBJECT
    QML_ELEMENT

    Q_PROPERTY(QVariantMap global READ global WRITE setGlobal NOTIFY globalChanged FINAL)
    Q_PROPERTY(QVariantMap plugins READ plugins WRITE setPlugins NOTIFY pluginsChanged FINAL)

public:
    SettingsSetQuery(QObject* parent = nullptr);

    auto global() const -> const QVariantMap&;
    void setGlobal(const QVariantMap& v);

    auto plugins() const -> const QVariantMap&;
    void setPlugins(const QVariantMap& v);

    void reload() override;

    Q_SIGNAL void globalChanged();
    Q_SIGNAL void pluginsChanged();

private:
    QVariantMap m_global;
    QVariantMap m_plugins;
};

} // namespace waywallen
