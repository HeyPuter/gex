/* GObject's calls through function pointers of more parameters than the
 * function takes, which wasm traps on unless the caller cuts them as C on
 * a real machine does: G_DEFINE_TYPE's class and instance initialisers
 * (GLib passes two arguments, they take one), and a signal handler that
 * leaves out the user data. One line per check, compared with native. */
#include <glib-object.h>
#include <stdio.h>

typedef struct { GObject parent; int x; } Foo;
typedef struct { GObjectClass parent; } FooClass;
G_DEFINE_TYPE(Foo, foo, G_TYPE_OBJECT)

static guint poke;

static void foo_class_init(FooClass *k)
{
    printf("class_init %s\n", G_OBJECT_CLASS_TYPE(k) == foo_get_type() ? "ok" : "wrong");
    poke = g_signal_new("poke", G_TYPE_FROM_CLASS(k), G_SIGNAL_RUN_LAST, 0, NULL, NULL,
                        g_cclosure_marshal_VOID__VOID, G_TYPE_NONE, 0);
}

static void foo_init(Foo *f) { f->x = 7; }

static int handled;
/* no user_data parameter */
static void on_poke(Foo *f) { handled += f->x; }

static void each(gpointer data) { handled += GPOINTER_TO_INT(data); }

int main(void)
{
    Foo *f = g_object_new(foo_get_type(), NULL);
    printf("init x %d\n", f->x);
    g_signal_connect(f, "poke", G_CALLBACK(on_poke), NULL);
    g_signal_emit(f, poke, 0);
    printf("handler %d\n", handled);
    /* a GFunc (data, user_data) that takes one */
    GList *l = g_list_append(g_list_append(NULL, GINT_TO_POINTER(1)), GINT_TO_POINTER(2));
    handled = 0;
    g_list_foreach(l, (GFunc)each, NULL);
    printf("foreach %d\n", handled);
    g_list_free(l);
    g_object_unref(f);
    return 0;
}
