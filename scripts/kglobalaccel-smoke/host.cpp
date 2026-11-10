// Headless host for the real kglobalacceld library (what KWin embeds):
// owns org.kde.kglobalaccel on the session bus; stdin "press" feeds an Escape
// press+release through keyEvent() like KWin does and prints whether
// kglobalaccel consumed it.
#include <QCoreApplication>
#include <QSocketNotifier>
#include <QTextStream>
#include <cstdio>
#include <unistd.h>
#include <kglobalacceld.h>
int main(int argc, char **argv) {
    QCoreApplication app(argc, argv);
    auto *d = new KGlobalAccelD();
    if (!d->init()) { fprintf(stderr, "init failed\n"); return 1; }
    printf("ready\n"); fflush(stdout);
    QSocketNotifier n(STDIN_FILENO, QSocketNotifier::Read);
    QObject::connect(&n, &QSocketNotifier::activated, [&]() {
        char buf[256];
        ssize_t r = read(STDIN_FILENO, buf, sizeof buf - 1);
        if (r <= 0) { delete d; app.quit(); return; }
        buf[r] = 0;
        QString cmd = QString::fromLatin1(buf).trimmed();
        for (const QString &c : cmd.split('\n')) {
            if (c == "press") {
                bool h = d->keyEvent(Qt::Key_Escape, ShortcutKeyState::Pressed);
                d->keyEvent(Qt::Key_Escape, ShortcutKeyState::Released);
                printf("handled=%d\n", h ? 1 : 0); fflush(stdout);
            } else if (c == "quit") { delete d; app.quit(); }
        }
    });
    return app.exec();
}
