module;
#include "QExtra/macro_qt.hpp"
#include <QtCore/qnamespace.h>
#include <QtCore/qtypes.h>

#ifdef Q_MOC_RUN
#    include "waywallen/model/user_property_model.moc"
#endif

export module waywallen:model.user_property;
export import qextra;
import rstd.cppstd;

export namespace waywallen::model
{

// QML-side list model for `general.properties` published by a
// wescene-renderer subprocess. The view (WallpaperPage right pane)
// uses this as a ListView model; the panel-level QML logic that used
// to live in UserPropertyPanel.qml (schema parse / order sort / wire
// value coercion / 200ms debounce) is consolidated here.
//
// `schemaJson` is the renderer-published map<string,WPProperty>;
// `overridesJson` is the DB column verbatim (object keyed by property
// name with raw wire-side string values).
class UserPropertyListModel : public QAbstractListModel {
    Q_OBJECT
    QML_ELEMENT

    Q_PROPERTY(QString schemaJson    READ schemaJson    WRITE setSchemaJson    NOTIFY schemaJsonChanged)
    Q_PROPERTY(QString overridesJson READ overridesJson WRITE setOverridesJson NOTIFY overridesJsonChanged)
    Q_PROPERTY(int     count         READ rowCount                             NOTIFY countChanged)

public:
    enum Roles {
        KeyRole = Qt::UserRole + 1,
        LabelRole,
        TypeRole,
        SupportedRole,
        MinValRole,
        MaxValRole,
        CurrentValueRole,
        HasAlphaRole,
    };
    Q_ENUM(Roles)

    explicit UserPropertyListModel(QObject* parent = nullptr);
    ~UserPropertyListModel() override;

    int      rowCount(const QModelIndex& parent = {}) const override;
    QVariant data(const QModelIndex& index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    auto schemaJson() const -> const QString& { return m_schema_json; }
    void setSchemaJson(const QString& v);
    Q_SIGNAL void schemaJsonChanged();

    auto overridesJson() const -> const QString& { return m_overrides_json; }
    void setOverridesJson(const QString& v);
    Q_SIGNAL void overridesJsonChanged();

    Q_SIGNAL void countChanged();

    // Mutate the local value for a single key. Internal state +
    // `dataChanged` + `valueChanged` all fire synchronously. UI
    // controls bind to roles and react to `dataChanged`; the wire
    // push path (daemon RPC) is driven by `valueChanged` so it
    // can debounce without seeing the noise from external schema /
    // overrides rebuilds.
    Q_INVOKABLE void setValue(const QString& key, const QString& value);

    // `setValue` every key back to its schema default. One
    // `valueChanged` per key, in order.
    Q_INVOKABLE void resetAll();

    // Emitted exclusively from `setValue` / `resetAll` (i.e. user
    // intent), never from external schema/overrides updates. Drives
    // the QML-side debounced query flush.
    Q_SIGNAL void valueChanged(const QString& key, const QString& value);

private:
    struct Entry {
        QString key;
        QString label;
        QString type;
        bool    supported { false };
        double  min_val { 0.0 };
        double  max_val { 1.0 };
        QString default_wire;
        double  order { 0.0 };
    };

    void rebuildEntries_();
    QString currentValueFor_(qsizetype row) const;
    void    notifyCurrentChanged_(const QString& key);

    QString                 m_schema_json;
    QString                 m_overrides_json;
    QHash<QString, QString> m_overrides; // parsed view of m_overrides_json
    QList<Entry>            m_entries;
};

} // namespace waywallen::model
