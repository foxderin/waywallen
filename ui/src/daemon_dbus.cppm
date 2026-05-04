module;
#include "QExtra/macro_qt.hpp"

#ifdef Q_MOC_RUN
#    include "waywallen/daemon_dbus.moc"
#endif

#include <QtCore/QVariantList>
#include <QtCore/QVariantMap>
#include <QtDBus/QDBusConnection>
#include <QtDBus/QDBusMessage>
#include <QtDBus/QDBusServiceWatcher>
#include <QtDBus/QDBusVariant>

export module waywallen:daemon_dbus;
export import qextra;

export namespace waywallen
{

class DaemonDBusClient : public QObject {
    Q_OBJECT
    QML_ELEMENT
    QML_SINGLETON

public:
    /// Single source of truth for the daemon's reachability + compatibility.
    /// All UI bindings read this; per-flag bool variables are deliberately
    /// avoided so the state machine has one anchor.
    enum Status {
        Disconnected,    ///< Service not on bus, or DBus call failed.
        VersionMissing,  ///< Daemon online but lacks Version property (old build).
        VersionMismatch, ///< Daemon's Version differs from kRequiredDaemonVersion.
        Connected,       ///< WsPort + Version both ok and compatible.
    };
    Q_ENUM(Status)

    Q_PROPERTY(Status   status         READ status         NOTIFY statusChanged FINAL)
    Q_PROPERTY(quint16  wsPort         READ wsPort         NOTIFY wsPortChanged FINAL)
    Q_PROPERTY(QString  daemonVersion  READ daemonVersion  NOTIFY statusChanged FINAL)
    /// Convenience derived from `status == Connected`.
    Q_PROPERTY(bool     daemonAvailable READ daemonAvailable NOTIFY statusChanged FINAL)

    explicit DaemonDBusClient(QObject* parent = nullptr);
    ~DaemonDBusClient() override;

    static DaemonDBusClient* create(QQmlEngine*, QJSEngine*);
    static DaemonDBusClient* instance();

    Status        status() const          { return m_status; }
    quint16       wsPort() const          { return m_ws_port; }
    const QString& daemonVersion() const  { return m_daemon_version; }
    bool          daemonAvailable() const { return m_status == Connected; }

    /// Synchronous round-trip: read WsPort, then probe Version. Updates
    /// `status` to one of {Disconnected, VersionMissing, VersionMismatch,
    /// Connected}. Returns the freshly-read port (0 on failure).
    Q_INVOKABLE quint16 refreshWsPort();

    /// Spawn the daemon as a detached child. Returns true on success.
    Q_INVOKABLE bool launchDaemon();

    /// Enumerate processes whose /proc/<pid>/comm equals "waywallen". Each
    /// entry: { "pid": uint, "cmdline": string }. Used by the "daemon not
    /// run" dialog to surface zombies before relaunching.
    Q_INVOKABLE QVariantList listWaywallenProcesses();

    /// Send SIGTERM to `pid`. Returns true if kill(2) succeeded.
    Q_INVOKABLE bool killProcess(quint32 pid);

    Q_SIGNAL void statusChanged();
    Q_SIGNAL void wsPortChanged(quint16 port);

private:
    Q_SLOT void on_service_registered(const QString& service);
    Q_SLOT void on_service_unregistered(const QString& service);
    Q_SLOT void on_ready();
    Q_SLOT void on_shutting_down();
    Q_SLOT void on_properties_changed(const QString&     iface,
                                      const QVariantMap& changed,
                                      const QStringList& invalidated);

    void setup_subscriptions();
    void set_status(Status s);
    void set_ws_port(quint16 port);

    /// `org.freedesktop.DBus.Properties.Get(kInterface, prop)`.
    QDBusMessage call_get(const QString& prop);

    QDBusConnection      m_bus;
    QDBusServiceWatcher* m_watcher { nullptr };
    quint16              m_ws_port { 0 };
    QString              m_daemon_version;
    Status               m_status { Disconnected };
};

} // namespace waywallen
