using System.Net;
using System.Text.Json;
using CounterApi.Activities;
using CounterApi.Domain;
using CounterApi.Features;
using CounterApi.Infrastructure.Data;
using CounterApi.Infrastructure.Gateways;
using CounterApi.Workflows;
using Dapr.Workflow;
using N8T.Infrastructure;
using N8T.Infrastructure.Controller;
using N8T.Infrastructure.EfCore;

var builder = WebApplication.CreateBuilder(args);

builder.Services.AddDaprWorkflow(options =>
{
    options.RegisterWorkflow<PlaceOrderWorkflow>();

    options.RegisterActivity<NotifyActivity>();
    options.RegisterActivity<AddOrderActivity>();
    options.RegisterActivity<BaristaUpdateOrderActivity>();
    options.RegisterActivity<KitchenUpdateOrderActivity>();
});

builder.WebHost
    .ConfigureKestrel(webBuilder =>
    {
        webBuilder.Listen(IPAddress.Any, builder.Configuration.GetValue("RestPort", 5002)); // REST
    });

builder.Services
    .AddHttpContextAccessor()
    .AddCustomMediatR(new[] { typeof(Order) })
    .AddCustomValidators(new[] { typeof(Order) });

builder.Services
    .AddPostgresDbContext<MainDbContext>(
        builder.Configuration.GetConnectionString("counterdb"),
        null,
        svc => svc.AddRepository(typeof(Repository<>)))
    .AddDatabaseDeveloperPageExceptionFilter();

builder.Services.AddScoped<IItemGateway, ItemDaprGateway>();
builder.Services.AddDaprClient();
builder.Services.AddSingleton(new JsonSerializerOptions()
{
    PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
    PropertyNameCaseInsensitive = true,
});

var app = builder.Build();

if (!app.Environment.IsDevelopment())
{
    app.UseExceptionHandler("/Error");
}

app.MapGet("/error", () => Results.Problem("An error occurred.", statusCode: 500))
    .ExcludeFromDescription();

app.UseMiddleware<ExceptionMiddleware>();

app.UseRouting();

app.UseCloudEvents();

app.MapGet("/", () => "Hello World!");

_ = app.MapOrderInApiRoutes()
    .MapOrderFulfillmentApiRoutes();

await app.DoDbMigrationAsync(app.Logger);

app.Run();