// City coordinates for the weather map, keyed by lowercase city name -> [lat, lon].
// Leaflet projects these directly; extend this table to cover more markets.
const CITY_COORDS = {
"new york": [
40.71,
-74.01
],
"chicago": [
41.84,
-87.68
],
"miami": [
25.79,
-80.29
],
"denver": [
39.74,
-104.99
],
"austin": [
30.27,
-97.74
],
"los angeles": [
34.05,
-118.24
],
"seattle": [
47.61,
-122.33
],
"phoenix": [
33.45,
-112.07
],
"boston": [
42.36,
-71.06
],
"dallas": [
32.85,
-96.85
],
"minneapolis": [
44.88,
-93.22
],
"houston": [
29.76,
-95.37
],
"philadelphia": [
39.95,
-75.16
],
"atlanta": [
33.75,
-84.39
],
"washington": [
38.85,
-77.04
],
"detroit": [
42.33,
-83.05
],
"san francisco": [
37.77,
-122.42
],
"sacramento": [
38.58,
-121.49
],
"portland": [
45.52,
-122.68
],
"las vegas": [
36.08,
-115.15
],
"salt lake city": [
40.76,
-111.89
],
"kansas city": [
39.1,
-94.58
],
"st louis": [
38.63,
-90.2
],
"nashville": [
36.12,
-86.68
],
"charlotte": [
35.23,
-80.84
],
"tampa": [
27.96,
-82.54
],
"orlando": [
28.43,
-81.31
],
"san antonio": [
29.42,
-98.49
],
"san diego": [
32.73,
-117.19
],
"baltimore": [
39.29,
-76.61
],
"cleveland": [
41.41,
-81.85
],
"pittsburgh": [
40.49,
-80.23
],
"cincinnati": [
39.05,
-84.67
],
"indianapolis": [
39.72,
-86.29
],
"columbus": [
39.99,
-82.89
],
"milwaukee": [
42.95,
-87.9
],
"oklahoma city": [
35.39,
-97.6
],
"new orleans": [
29.99,
-90.26
],
"raleigh": [
35.89,
-78.78
]
};
