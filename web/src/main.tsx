const publicRoutes = ["/case", "/case/updates", "/case/identity"];
if (publicRoutes.includes(location.pathname.replace(/\/$/, ""))) {
  void import("./public/PublicApp");
} else {
  void import("./backoffice");
}
