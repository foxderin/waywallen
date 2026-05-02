module;
#include "waywallen/query/settings_query.moc.h"
#undef assert
#include <rstd/macro.hpp>

module waywallen;
import :query.settings;
import :app;

using namespace Qt::Literals::StringLiterals;
using namespace qextra::prelude;

namespace proto = waywallen::control::v1;

namespace waywallen
{

namespace
{

auto layout_to_map(const proto::LayoutPrefs& l) -> QVariantMap {
    QVariantMap m;
    m[u"fillmode"_s] = static_cast<int>(l.fillmode());
    m[u"align"_s]    = static_cast<int>(l.align());
    QVariantList rgba;
    for (auto v : l.clearRgba()) {
        rgba.append(QVariant(v));
    }
    m[u"clearRgba"_s] = rgba;
    return m;
}

auto map_to_layout(const QVariantMap& m) -> proto::LayoutPrefs {
    proto::LayoutPrefs l;
    l.setFillmode(static_cast<proto::FillMode>(m.value(u"fillmode"_s).toInt()));
    l.setAlign(static_cast<proto::Align>(m.value(u"align"_s).toInt()));
    QList<float> rgba;
    for (const auto& v : m.value(u"clearRgba"_s).toList()) {
        rgba.append(v.toFloat());
    }
    l.setClearRgba(rgba);
    return l;
}

auto global_to_map(const proto::GlobalSettings& g) -> QVariantMap {
    QVariantMap m;
    m[u"defaultWidth"_s]  = g.defaultWidth();
    m[u"defaultHeight"_s] = g.defaultHeight();
    if (g.hasLayoutDefaults()) {
        m[u"layoutDefaults"_s] = layout_to_map(g.layoutDefaults());
    }
    return m;
}

auto plugins_to_map(const proto::SettingsGetResponse::PluginsEntry& src) -> QVariantMap {
    QVariantMap out;
    for (auto it = src.constBegin(); it != src.constEnd(); ++it) {
        QVariantMap inner;
        const auto& values = it.value().values();
        for (auto vit = values.constBegin(); vit != values.constEnd(); ++vit) {
            inner[vit.key()] = vit.value();
        }
        out[it.key()] = inner;
    }
    return out;
}

auto map_to_global(const QVariantMap& m) -> proto::GlobalSettings {
    proto::GlobalSettings g;
    g.setDefaultWidth(m.value(u"defaultWidth"_s).toUInt());
    g.setDefaultHeight(m.value(u"defaultHeight"_s).toUInt());
    // Round-trip layout_defaults so a single-plugin SettingsSet doesn't
    // wipe the daemon's current LayoutPrefs (fillmode / align /
    // clear_rgba). UI never edits these — it just forwards them.
    if (m.contains(u"layoutDefaults"_s)) {
        g.setLayoutDefaults(map_to_layout(m.value(u"layoutDefaults"_s).toMap()));
    }
    return g;
}

auto map_to_plugins(const QVariantMap& m) -> QHash<QString, proto::PluginSettings> {
    QHash<QString, proto::PluginSettings> out;
    for (auto it = m.constBegin(); it != m.constEnd(); ++it) {
        proto::PluginSettings ps;
        proto::PluginSettings::ValuesEntry values;
        const auto inner = it.value().toMap();
        for (auto vit = inner.constBegin(); vit != inner.constEnd(); ++vit) {
            values.insert(vit.key(), vit.value().toString());
        }
        ps.setValues(values);
        out.insert(it.key(), ps);
    }
    return out;
}

} // namespace

// ---------------------------------------------------------------------------
// SettingsGetQuery
// ---------------------------------------------------------------------------

SettingsGetQuery::SettingsGetQuery(QObject* parent): Query(parent) {}

auto SettingsGetQuery::global() const -> const QVariantMap& { return m_global; }
auto SettingsGetQuery::plugins() const -> const QVariantMap& { return m_plugins; }

void SettingsGetQuery::reload() {
    setStatus(Status::Querying);
    auto backend = App::instance()->backend();

    auto req = proto::Request {};
    req.setSettingsGet(proto::SettingsGetRequest {});

    auto self = QWatcher { this };
    spawn([self, backend, req = std::move(req)]() mutable -> task<void> {
        auto result = co_await backend->send(std::move(req));
        co_await asio::post(asio::bind_executor(self->get_executor(), use_task));
        if (! self) co_return;

        self->inspect_set(result, [self](const proto::Response& rsp) {
            const auto& get_rsp = rsp.settingsGet();
            self->m_global  = global_to_map(get_rsp.global());
            self->m_plugins = plugins_to_map(get_rsp.plugins());
            Q_EMIT self->globalChanged();
            Q_EMIT self->pluginsChanged();
        });
        co_return;
    });
}

// ---------------------------------------------------------------------------
// SettingsSetQuery
// ---------------------------------------------------------------------------

SettingsSetQuery::SettingsSetQuery(QObject* parent): Query(parent) {}

auto SettingsSetQuery::global() const -> const QVariantMap& { return m_global; }
void SettingsSetQuery::setGlobal(const QVariantMap& v) {
    if (m_global != v) {
        m_global = v;
        Q_EMIT globalChanged();
    }
}

auto SettingsSetQuery::plugins() const -> const QVariantMap& { return m_plugins; }
void SettingsSetQuery::setPlugins(const QVariantMap& v) {
    if (m_plugins != v) {
        m_plugins = v;
        Q_EMIT pluginsChanged();
    }
}

void SettingsSetQuery::reload() {
    setStatus(Status::Querying);
    auto backend = App::instance()->backend();

    auto req   = proto::Request {};
    auto inner = proto::SettingsSetRequest {};
    inner.setGlobal(map_to_global(m_global));
    inner.setPlugins(map_to_plugins(m_plugins));
    req.setSettingsSet(std::move(inner));

    auto self = QWatcher { this };
    spawn([self, backend, req = std::move(req)]() mutable -> task<void> {
        auto result = co_await backend->send(std::move(req));
        co_await asio::post(asio::bind_executor(self->get_executor(), use_task));
        if (! self) co_return;

        self->inspect_set(result, [](const proto::Response&) {});
        co_return;
    });
}

} // namespace waywallen

#include "waywallen/query/settings_query.moc.cpp"
