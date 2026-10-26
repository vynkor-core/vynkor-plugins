import machine
import time

# Ctrl-C in this window drops to the REPL before app.py starts the
# watchdog, which cannot be stopped and would reset an idle REPL in ~2 s.
maintenance = False
try:
    time.sleep(3)
except KeyboardInterrupt:
    maintenance = True
    print('maintenance: app not started, watchdog off')

if not maintenance:
    try:
        import app
        app.main()
    except KeyboardInterrupt:
        pass
    except Exception as e:
        try:
            import sys
            with open('crash.txt', 'w') as f:
                sys.print_exception(e, f)
        except Exception:
            pass
        time.sleep(10)
        machine.reset()
