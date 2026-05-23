module;
#include "waywallen/model/user_property_model.moc.h"

module waywallen;
import :model.user_property;

namespace waywallen::model
{

namespace {

bool isSupported(const QString& type) {
    return type == QLatin1String("color") || type == QLatin1String("slider") ||
           type == QLatin1String("bool");
}

QString jsonValueToWireString(const QJsonValue& v) {
    switch (v.type()) {
    case QJsonValue::Bool:
        return v.toBool() ? QStringLiteral("true") : QStringLiteral("false");
    case QJsonValue::Double:
        return QString::number(v.toDouble());
    case QJsonValue::String:
        return v.toString();
    case QJsonValue::Array: {
        QStringList parts;
        const auto a = v.toArray();
        parts.reserve(a.size());
        for (const auto& e : a)
            parts << QString::number(e.toDouble(), 'f', 4);
        return parts.join(QLatin1Char(' '));
    }
    default:
        return {};
    }
}

QString coerceDefaultWireString(const QJsonValue& def, const QString& type) {
    // For colors WE may emit the default either as `"r g b"` string or as
    // a JSON array; normalise to space-separated floats either way.
    if (type == QLatin1String("color")) {
        if (def.isArray()) {
            QStringList parts;
            const auto a = def.toArray();
            parts.reserve(a.size());
            for (const auto& e : a)
                parts << QString::number(e.toDouble(), 'f', 4);
            return parts.join(QLatin1Char(' '));
        }
        if (def.isString()) return def.toString();
    }
    if (type == QLatin1String("bool"))
        return def.toBool() ? QStringLiteral("true") : QStringLiteral("false");
    if (type == QLatin1String("slider"))
        return QString::number(def.toDouble());
    return jsonValueToWireString(def);
}

} // namespace

UserPropertyListModel::UserPropertyListModel(QObject* parent)
    : QAbstractListModel(parent) {}

UserPropertyListModel::~UserPropertyListModel() = default;

int UserPropertyListModel::rowCount(const QModelIndex& parent) const {
    if (parent.isValid()) return 0;
    return static_cast<int>(m_entries.size());
}

QHash<int, QByteArray> UserPropertyListModel::roleNames() const {
    return {
        { KeyRole,          "key" },
        { LabelRole,        "label" },
        { TypeRole,         "type" },
        { SupportedRole,    "supported" },
        { MinValRole,       "minVal" },
        { MaxValRole,       "maxVal" },
        { CurrentValueRole, "currentValue" },
        { HasAlphaRole,     "hasAlpha" },
    };
}

QVariant UserPropertyListModel::data(const QModelIndex& index, int role) const {
    if (! index.isValid()) return {};
    const auto row = index.row();
    if (row < 0 || row >= m_entries.size()) return {};
    const auto& e = m_entries.at(row);
    switch (role) {
    case KeyRole:          return e.key;
    case LabelRole:        return e.label;
    case TypeRole:         return e.type;
    case SupportedRole:    return e.supported;
    case MinValRole:       return e.min_val;
    case MaxValRole:       return e.max_val;
    case CurrentValueRole: return currentValueFor_(row);
    case HasAlphaRole: {
        const QString cv = currentValueFor_(row);
        static const QRegularExpression reSpaces(QStringLiteral("\\s+"));
        return cv.trimmed().split(reSpaces, Qt::SkipEmptyParts).size() >= 4;
    }
    default: return {};
    }
}

QString UserPropertyListModel::currentValueFor_(qsizetype row) const {
    const auto& e = m_entries.at(row);
    const auto  it = m_overrides.constFind(e.key);
    if (it != m_overrides.constEnd() && ! it.value().isEmpty()) return it.value();
    return e.default_wire;
}

void UserPropertyListModel::setSchemaJson(const QString& v) {
    if (v == m_schema_json) return;
    m_schema_json = v;
    Q_EMIT schemaJsonChanged();
    rebuildEntries_();
}

void UserPropertyListModel::setOverridesJson(const QString& v) {
    if (v == m_overrides_json) return;
    m_overrides_json = v;
    Q_EMIT overridesJsonChanged();

    m_overrides.clear();
    if (! m_overrides_json.isEmpty()) {
        QJsonParseError err {};
        const auto doc = QJsonDocument::fromJson(m_overrides_json.toUtf8(), &err);
        if (err.error == QJsonParseError::NoError && doc.isObject()) {
            const auto obj = doc.object();
            for (auto it = obj.constBegin(); it != obj.constEnd(); ++it) {
                if (it.value().isString())
                    m_overrides.insert(it.key(), it.value().toString());
            }
        }
    }
    // Every row's CurrentValue derivation depends on m_overrides.
    if (! m_entries.isEmpty()) {
        Q_EMIT dataChanged(index(0), index(static_cast<int>(m_entries.size()) - 1),
                           { CurrentValueRole, HasAlphaRole });
    }
}

void UserPropertyListModel::rebuildEntries_() {
    beginResetModel();
    m_entries.clear();
    if (! m_schema_json.isEmpty()) {
        QJsonParseError err {};
        const auto doc = QJsonDocument::fromJson(m_schema_json.toUtf8(), &err);
        if (err.error == QJsonParseError::NoError && doc.isObject()) {
            const auto obj = doc.object();
            m_entries.reserve(obj.size());
            for (auto it = obj.constBegin(); it != obj.constEnd(); ++it) {
                const auto v = it.value().toObject();
                Entry e;
                e.key   = it.key();
                e.label = v.value(QStringLiteral("text")).toString();
                if (e.label.isEmpty()) e.label = e.key;
                e.type  = v.value(QStringLiteral("type")).toString().toLower();
                e.supported = isSupported(e.type);
                e.min_val   = v.value(QStringLiteral("min")).toDouble(0.0);
                e.max_val   = v.value(QStringLiteral("max")).toDouble(1.0);
                e.default_wire =
                    coerceDefaultWireString(v.value(QStringLiteral("value")), e.type);
                e.order = v.value(QStringLiteral("order")).toDouble(0.0);
                m_entries.append(std::move(e));
            }
            std::sort(m_entries.begin(), m_entries.end(),
                      [](const Entry& a, const Entry& b) { return a.order < b.order; });
        }
    }
    endResetModel();
    Q_EMIT countChanged();
}

void UserPropertyListModel::setValue(const QString& key, const QString& value) {
    m_overrides.insert(key, value);
    notifyCurrentChanged_(key);
    Q_EMIT valueChanged(key, value);
}

void UserPropertyListModel::resetAll() {
    for (const auto& e : m_entries) {
        m_overrides.insert(e.key, e.default_wire);
        notifyCurrentChanged_(e.key);
        Q_EMIT valueChanged(e.key, e.default_wire);
    }
}

void UserPropertyListModel::notifyCurrentChanged_(const QString& key) {
    for (qsizetype i = 0; i < m_entries.size(); ++i) {
        if (m_entries.at(i).key == key) {
            const auto idx = index(static_cast<int>(i));
            Q_EMIT dataChanged(idx, idx, { CurrentValueRole, HasAlphaRole });
            return;
        }
    }
}

} // namespace waywallen::model

#include "waywallen/model/user_property_model.moc.cpp"
