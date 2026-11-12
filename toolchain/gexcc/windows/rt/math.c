/* C99 math functions the CRT (msvcrt, ucrtbase) exports only under another
 * name, as MinGW's libmingwex provides them (gexcc-win). */
float _hypotf(float x, float y);

float hypotf(float x, float y)
{
    return _hypotf(x, y);
}
